//! Recursos dos vínculos (`ServiceLink::resource`): no deploy do consumidor, antes dos pré-deploy
//! checks, garante que cada bucket ou database existe no provedor. Idempotente. Ver
//! `docs/plano-recursos-e-vinculos.md` §7.

use super::*;
use crate::domain::{LinkResource, ResourceKind, ServiceSource, resource_kind};
use crate::features::databases::shared_db::Engine;
use std::collections::HashMap;

/// Quanto o provisionamento de um recurso pode levar antes de o deploy desistir com a causa.
const PROVISION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl DeployExecutor {
    /// Garante os recursos de todos os vínculos do serviço. Erro = o deploy para aqui (nada foi tocado).
    pub(super) async fn ensure_link_resources(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<()> {
        for link in &svc.spec.links {
            let Some(resource) = &link.resource else {
                continue;
            };
            let target = crate::store::services::get(&self.db, &link.service_id)
                .await?
                .ok_or_else(|| anyhow!("o serviço vinculado {} não existe", link.service_id))?;
            let kind = resource_kind(target.spec.product.as_deref())
                .ok_or_else(|| anyhow!("\"{}\" não oferece recursos", target.spec.name))?;
            let container = self.provider_container(&target).await.ok_or_else(|| {
                anyhow!(
                    "\"{}\" não está no ar: o recurso \"{}\" precisa dele rodando (faça o deploy com \
                     dependências)",
                    target.spec.name,
                    resource.name
                )
            })?;
            let target_env: HashMap<String, String> =
                super::super::env_resolve::resolve_own(&self.db, &self.secrets, &target)
                    .await?
                    .into_iter()
                    .collect();
            let created = match kind {
                ResourceKind::Bucket => {
                    self.ensure_bucket(&target, &container, &target_env, resource)
                        .await?
                }
                ResourceKind::Database => tokio::time::timeout(
                    PROVISION_TIMEOUT,
                    ensure_database(&target, &container, &resource.name),
                )
                .await
                .map_err(|_| {
                    anyhow!(
                        "database \"{}\": \"{}\" não respondeu em {}s",
                        resource.name,
                        target.spec.name,
                        PROVISION_TIMEOUT.as_secs()
                    )
                })??,
            };
            let what = match kind {
                ResourceKind::Bucket => "bucket",
                ResourceKind::Database => "database",
            };
            let line = if created {
                format!(
                    "==> {what} \"{}\" criado em {}",
                    resource.name, target.spec.name
                )
            } else {
                format!(
                    "==> {what} \"{}\" já existia em {}",
                    resource.name, target.spec.name
                )
            };
            self.log_step(&dep.id, &svc.id, &line).await;
            if resource.public_read {
                self.log_step(
                    &dep.id,
                    &svc.id,
                    &format!("==> bucket \"{}\" com leitura pública", resource.name),
                )
                .await;
            }
        }
        Ok(())
    }

    /// O container do provedor: o alvo da stack (Compose) ou a primeira réplica no ar.
    async fn provider_container(&self, target: &Service) -> Option<String> {
        if matches!(target.spec.source, ServiceSource::Compose(_)) {
            return crate::features::deploy::shared_net::server_container(
                &self.docker.inner,
                target,
            )
            .await;
        }
        containers::find_live_replicas(&self.docker.inner, &target.id)
            .await
            .ok()?
            .into_iter()
            .find(|r| r.running)
            .map(|r| r.id)
    }

    /// Bucket: pela API S3 do próprio provedor, no IP do container na rede do projeto.
    async fn ensure_bucket(
        &self,
        target: &Service,
        container: &str,
        env: &HashMap<String, String>,
        resource: &LinkResource,
    ) -> Result<bool> {
        let network = self.network_name(&target.spec.project_id).await?;
        let ip = containers::get_container_ip(&self.docker.inner, container, &network).await?;
        let (access_key, secret_key) = s3_credentials(target.spec.product.as_deref(), env)
            .ok_or_else(|| {
                anyhow!(
                    "não achei as credenciais do S3 em \"{}\" (RUSTFS_ACCESS_KEY/RUSTFS_SECRET_KEY ou \
                     MINIO_ROOT_USER/MINIO_ROOT_PASSWORD)",
                    target.spec.name
                )
            })?;
        let cfg = crate::platform::s3::S3Config {
            endpoint: format!("http://{ip}:{}", target.spec.port),
            region: "us-east-1".into(),
            bucket: resource.name.clone(),
            access_key,
            secret_key,
            path_style: true,
        };
        // Teto curto: o cliente S3 do backup tenta 3 vezes com 300 s cada, o que penduraria o deploy
        // por 15 minutos com o provedor inalcançável.
        let endpoint = cfg.endpoint.clone();
        let work = async {
            let created = crate::platform::s3::ensure_bucket(&cfg).await?;
            if resource.public_read {
                crate::platform::s3::set_public_read(&cfg).await?;
            }
            anyhow::Ok(created)
        };
        tokio::time::timeout(PROVISION_TIMEOUT, work)
            .await
            .map_err(|_| {
                anyhow!(
                    "bucket \"{}\": o S3 de \"{}\" não respondeu em {}s ({endpoint})",
                    resource.name,
                    target.spec.name,
                    PROVISION_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| anyhow!("bucket \"{}\": {e}", resource.name))
    }
}

/// As credenciais de administrador de um S3 local, pelas convenções de cada imagem.
pub(super) fn s3_credentials(
    product: Option<&str>,
    env: &HashMap<String, String>,
) -> Option<(String, String)> {
    let get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
    let pairs: &[(&str, &str)] = match product.map(str::to_ascii_lowercase).as_deref() {
        Some("rustfs") => &[("RUSTFS_ACCESS_KEY", "RUSTFS_SECRET_KEY")],
        Some("minio") => &[
            ("MINIO_ROOT_USER", "MINIO_ROOT_PASSWORD"),
            ("MINIO_ACCESS_KEY", "MINIO_SECRET_KEY"),
        ],
        _ => &[],
    };
    pairs
        .iter()
        .find_map(|(user, pass)| Some((get(user)?, get(pass)?)))
}

/// Database: SQL no cliente de administração dentro do container. O nome já passou por
/// `is_valid_resource_name` (só `[a-z][a-z0-9_]*`), então pode entrar no SQL.
async fn ensure_database(target: &Service, container: &str, name: &str) -> Result<bool> {
    let engine = Engine::from_kind(target.spec.product.as_deref())
        .ok_or_else(|| anyhow!("\"{}\" não é um banco conhecido", target.spec.name))?;
    let (sh, exists_sql, create_sql) = database_scripts(engine, name)?;
    let found = crate::features::databases::shared_db::exec_sh(engine, container, sh, &exists_sql)
        .await
        .map_err(|e| anyhow!("database \"{name}\": {e}"))?;
    if !found.trim().is_empty() {
        return Ok(false);
    }
    crate::features::databases::shared_db::exec_sh(engine, container, sh, &create_sql)
        .await
        .map_err(|e| anyhow!("database \"{name}\": {e}"))?;
    Ok(true)
}

/// `(sh do cliente em modo "só valores", SQL que imprime algo se existe, SQL que cria)`.
pub(super) fn database_scripts(
    engine: Engine,
    name: &str,
) -> Result<(&'static str, String, String)> {
    match engine {
        Engine::Postgres => Ok((
            r#"exec psql -U "$POSTGRES_USER" -d postgres -v ON_ERROR_STOP=1 -qtA"#,
            format!("SELECT 1 FROM pg_database WHERE datname = '{name}';\n"),
            format!("CREATE DATABASE \"{name}\";\n"),
        )),
        Engine::MySql | Engine::MariaDb => Ok((
            r#"B=$(command -v mariadb || command -v mysql); MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec "$B" -uroot -N -B"#,
            format!(
                "SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = '{name}';\n"
            ),
            format!("CREATE DATABASE IF NOT EXISTS `{name}`;\n"),
        )),
        Engine::Mongo => Err(anyhow!("MongoDB cria o database sozinho no primeiro uso")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credenciais_s3_por_imagem() {
        let env: HashMap<String, String> = [
            ("RUSTFS_ACCESS_KEY".to_string(), "ak".to_string()),
            ("RUSTFS_SECRET_KEY".to_string(), "sk".to_string()),
        ]
        .into();
        assert_eq!(
            s3_credentials(Some("rustfs"), &env),
            Some(("ak".into(), "sk".into()))
        );
        assert_eq!(s3_credentials(Some("minio"), &env), None);
        let minio: HashMap<String, String> = [
            ("MINIO_ROOT_USER".to_string(), "u".to_string()),
            ("MINIO_ROOT_PASSWORD".to_string(), "p".to_string()),
        ]
        .into();
        assert_eq!(
            s3_credentials(Some("minio"), &minio),
            Some(("u".into(), "p".into()))
        );
        assert_eq!(s3_credentials(Some("postgres"), &minio), None);
    }

    #[test]
    fn scripts_de_database_conferem_antes_de_criar() {
        let (sh, exists, create) = database_scripts(Engine::Postgres, "loja").unwrap();
        assert!(sh.contains("-qtA"), "só valores: vazio quando não existe");
        assert_eq!(
            exists,
            "SELECT 1 FROM pg_database WHERE datname = 'loja';\n"
        );
        assert_eq!(create, "CREATE DATABASE \"loja\";\n");
        let (_, _, create) = database_scripts(Engine::MySql, "loja").unwrap();
        assert_eq!(create, "CREATE DATABASE IF NOT EXISTS `loja`;\n");
        assert!(database_scripts(Engine::Mongo, "loja").is_err());
    }

    #[test]
    fn politica_de_leitura_publica_so_libera_ler() {
        let p = crate::platform::s3::public_read_policy("uploads");
        let v: serde_json::Value = serde_json::from_str(&p).unwrap();
        assert_eq!(v["Statement"][0]["Action"][0], "s3:GetObject");
        assert_eq!(v["Statement"][0]["Resource"][0], "arn:aws:s3:::uploads/*");
    }
}
