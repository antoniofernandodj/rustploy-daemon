//! Resolução de env vars com secrets decifradas — extraído de
//! `DeployExecutor::resolve_env` pra ser reaproveitado também pelo `JobRunner`
//! (`crates/daemon/src/features/jobs/runner.rs`), que precisa das mesmas env vars de
//! base (projeto + serviço gatilho, secrets incluídas) sem instanciar um
//! `DeployExecutor` inteiro.

use crate::domain::{EnvVarValue, Job, Service};
use crate::platform::secrets::SecretsManager;
use crate::store::Db;
use anyhow::Result;
use std::collections::HashMap;
use tracing::{debug, error, warn};

/// Resolve o valor de uma env `Secret(nome)`. **Secret ausente é erro** (antes virava string
/// vazia em silêncio e o container subia com a variável vazia). A mensagem diz qual secret
/// falta, para qual env e onde foi procurado, e some junto com o deploy/job que a usou.
///
/// Escopo: com `service_id`, o secret **do serviço** vale antes do **do projeto**; sem ele (env de
/// projeto, job autônomo), só o do projeto.
async fn secret_value(
    secrets: &SecretsManager,
    project_id: &str,
    service_id: Option<&str>,
    secret_name: &str,
    env_key: &str,
    owner: &str,
) -> Result<String> {
    secrets
        .resolve(project_id, service_id, secret_name)
        .await
        .map_err(|e| {
            let onde = match service_id {
                Some(sid) => format!("nos secrets do serviço '{sid}' e do projeto '{project_id}'"),
                None => format!("nos secrets do projeto '{project_id}'"),
            };
            anyhow::anyhow!(
                "a env '{env_key}' ({owner}) aponta para o secret '{secret_name}', que não pôde ser resolvido \
                 (procurado {onde}): {e}"
            )
        })
}

/// Só as env vars do projeto (base), secrets decifradas — sem nenhuma
/// específica de serviço. Reaproveitado por `resolve` (que soma as do
/// serviço por cima) e por `resolve_project_only` (jobs autônomos, sem
/// serviço gatilho).
async fn resolve_project_env(
    db: &Db,
    secrets: &SecretsManager,
    project_id: &str,
) -> Result<HashMap<String, String>> {
    let project_env = match crate::store::projects::get(db, project_id).await {
        Ok(Some(project)) => project.env_vars,
        Ok(None) => {
            warn!(
                project_id,
                "resolve_env: projeto não encontrado no banco — env vars de projeto não serão injetadas"
            );
            vec![]
        }
        Err(e) => {
            error!(
                project_id,
                error = %e,
                "resolve_env: falha ao carregar projeto (possível erro de desserialização do JSON env_vars) — env vars de projeto não serão injetadas"
            );
            vec![]
        }
    };

    let mut env_map: HashMap<String, String> = HashMap::new();
    for ev in &project_env {
        let value = match &ev.value {
            EnvVarValue::Plain(v) => v.clone(),
            EnvVarValue::Secret(name) => {
                debug!(project_id, secret = %name, "resolve_env: desencriptando secret do projeto");
                secret_value(secrets, project_id, None, name, &ev.key, "projeto").await?
            }
        };
        env_map.insert(ev.key.clone(), value);
    }
    Ok(env_map)
}

/// Funde env vars do projeto (base) com as do serviço (sobrescreve por
/// chave), decifrando `EnvVarValue::Secret` via `secrets`. Mesma precedência
/// de `crate::domain::resolve_env_vars`, mas com secrets resolvidas em texto puro
/// (`crate::domain::resolve_env_vars` deixa `EnvVarValue` intacto).
pub async fn resolve(
    db: &Db,
    secrets: &SecretsManager,
    svc: &Service,
) -> Result<Vec<(String, String)>> {
    // Vínculos primeiro, com a menor precedência: o que o projeto e o serviço definem por cima ganha.
    let mut env_map: HashMap<String, String> =
        link_env(db, secrets, svc).await.into_iter().collect();
    env_map.extend(resolve_own(db, secrets, svc).await?);
    Ok(env_map.into_iter().collect())
}

/// As variáveis que os **vínculos** do serviço entregam (`<PREFIXO>_HOST`, `_PORT`, `_URL`). Um alvo
/// que sumiu é pulado com aviso (o deploy não falha por isso). Ver `docs/plano-recursos-e-vinculos.md`.
///
/// - **Mesmo projeto**: host = o nome interno do provedor; a URL usa as credenciais dele (as mesmas que
///   a aba Conexão mostra).
/// - **Banco compartilhado de outro projeto**: host = o alias global; a URL é a do **database
///   gerenciado deste projeto** nesse servidor (nunca a do superusuário do servidor). Sem database
///   gerenciado, ou com mais de um, só `HOST` e `PORT`.
/// - Provedor HTTP (S3, um app): `http://host:porta`, sem credenciais na URL.
pub async fn link_env(db: &Db, secrets: &SecretsManager, svc: &Service) -> Vec<(String, String)> {
    use crate::platform::connection::{self, ConnTarget};
    let mut out = Vec::new();
    for link in &svc.spec.links {
        let target = match crate::store::services::get(db, &link.service_id).await {
            Ok(Some(t)) => t,
            _ => {
                warn!(service_id = %svc.id, linked = %link.service_id, "vínculo para um serviço que não existe; ignorado");
                continue;
            }
        };
        let prefix = link
            .env_prefix
            .clone()
            .unwrap_or_else(|| crate::domain::link_env_prefix(&target.spec.name));
        let product = target.spec.product.as_deref();
        let port = target.spec.port;
        let other_project = target.spec.project_id != svc.spec.project_id;

        let host = if other_project {
            crate::features::deploy::shared_net::alias_of(&target)
        } else {
            match &target.spec.source {
                crate::domain::ServiceSource::Compose(c) => {
                    connection::compose_host(&c.content, c.ingress_service.as_deref(), product)
                        .unwrap_or_else(|| crate::domain::app_network_alias(&target.spec.name))
                }
                _ => crate::domain::app_network_alias(&target.spec.name),
            }
        };

        let url = if connection::scheme(product) == Some("http") {
            Some(format!("http://{host}:{port}"))
        } else if other_project {
            managed_url_for(db, &target, &svc.spec.project_id).await
        } else {
            // As credenciais do provedor, lidas sem os vínculos dele (não há recursão).
            match resolve_own(db, secrets, &target).await {
                Ok(target_env) => {
                    let target_env: HashMap<String, String> = target_env.into_iter().collect();
                    let (database, user, password, auth_source) =
                        connection::credentials(product, |k| target_env.get(k).cloned());
                    connection::scheme(product).map(|_| {
                        connection::connection_url(
                            product,
                            &ConnTarget {
                                host: &host,
                                port,
                                // Com um database de recurso, a URL aponta para ele.
                                database: link
                                    .resource
                                    .as_ref()
                                    .map(|r| r.name.as_str())
                                    .or(database.as_deref()),
                                user: user.as_deref(),
                                password: password.as_deref(),
                                auth_source,
                            },
                        )
                    })
                }
                Err(e) => {
                    warn!(service_id = %svc.id, linked = %target.id, error = %e, "vínculo: não consegui ler as variáveis do provedor; sem _URL");
                    None
                }
            }
        };

        out.push((format!("{prefix}_HOST"), host));
        out.push((format!("{prefix}_PORT"), port.to_string()));
        if let Some(url) = url {
            out.push((format!("{prefix}_URL"), url));
        }
        if let Some(resource) = &link.resource {
            match crate::domain::resource_kind(product) {
                Some(crate::domain::ResourceKind::Bucket) => {
                    out.push((format!("{prefix}_BUCKET"), resource.name.clone()))
                }
                Some(crate::domain::ResourceKind::Database) => {
                    out.push((format!("{prefix}_DATABASE"), resource.name.clone()))
                }
                None => {}
            }
        }
    }
    out
}

/// A URL do database gerenciado de `project_id` no servidor compartilhado `server`, se houver
/// exatamente um.
async fn managed_url_for(db: &Db, server: &Service, project_id: &str) -> Option<String> {
    let engine =
        crate::features::databases::shared_db::Engine::from_kind(server.spec.product.as_deref())?;
    let rows = crate::store::managed_database::list(db, &server.id)
        .await
        .ok()?;
    let mine: Vec<_> = rows.iter().filter(|r| r.project_id == project_id).collect();
    match mine.as_slice() {
        [only] => Some(
            crate::features::databases::managed_view::to_view(server, engine, only).connection_url,
        ),
        _ => None,
    }
}

/// Projeto + serviço, **sem** os vínculos (o que o serviço declara por si).
pub(crate) async fn resolve_own(
    db: &Db,
    secrets: &SecretsManager,
    svc: &Service,
) -> Result<Vec<(String, String)>> {
    let mut env_map = resolve_project_env(db, secrets, &svc.spec.project_id).await?;

    for ev in &svc.spec.env_vars {
        let value = match &ev.value {
            EnvVarValue::Plain(v) => v.clone(),
            EnvVarValue::Secret(name) => {
                debug!(service_id = %svc.id, secret = %name, "resolve_env: desencriptando secret do serviço");
                secret_value(
                    secrets,
                    &svc.spec.project_id,
                    Some(&svc.id),
                    name,
                    &ev.key,
                    "serviço",
                )
                .await?
            }
        };
        env_map.insert(ev.key.clone(), value);
    }

    let keys: Vec<&str> = env_map.keys().map(|k| k.as_str()).collect();
    tracing::info!(
        service_id = %svc.id,
        service_vars = svc.spec.env_vars.len(),
        total = env_map.len(),
        keys = ?keys,
        "resolve_env: vars resolvidas (projeto + serviço)"
    );

    Ok(env_map.into_iter().collect())
}

/// Só as env vars do projeto — usado por jobs sem serviço gatilho
/// (`Job::trigger_service_id: None`, job 100% autônomo). Sem acesso a env
/// vars/secrets exclusivas de um serviço específico.
pub async fn resolve_project_only(
    db: &Db,
    secrets: &SecretsManager,
    project_id: &str,
) -> Result<Vec<(String, String)>> {
    let env_map = resolve_project_env(db, secrets, project_id).await?;
    tracing::info!(
        project_id,
        total = env_map.len(),
        "resolve_env: vars resolvidas (só projeto, job autônomo sem serviço gatilho)"
    );
    Ok(env_map.into_iter().collect())
}

/// Env vars completas de um `Job`: base (projeto, + serviço gatilho quando
/// houver) por baixo, `job.env_vars` sobrescrevendo por cima — maior
/// precedência, pensado pra overrides específicos do job (ex.:
/// `FLOW_MYENV=test`) sem precisar duplicar toda a config do
/// projeto/serviço. Centraliza aqui a lógica que antes vivia inline em
/// `jobs::runner` (o `match trigger_service_id` pra escolher `resolve` vs
/// `resolve_project_only`).
pub async fn resolve_job(
    db: &Db,
    secrets: &SecretsManager,
    job: &Job,
) -> Result<Vec<(String, String)>> {
    let base = match &job.trigger_service_id {
        Some(sid) => {
            let svc = crate::store::services::get(db, sid)
                .await?
                .ok_or_else(|| anyhow::anyhow!("serviço gatilho não encontrado: {sid}"))?;
            resolve(db, secrets, &svc).await?
        }
        None => resolve_project_only(db, secrets, &job.project_id).await?,
    };
    let mut env_map: HashMap<String, String> = base.into_iter().collect();

    for ev in &job.env_vars {
        let value = match &ev.value {
            EnvVarValue::Plain(v) => v.clone(),
            EnvVarValue::Secret(name) => {
                debug!(job_id = %job.id, secret = %name, "resolve_env: desencriptando secret do job");
                secrets
                    .resolve_for_job(
                        &job.project_id,
                        &job.id,
                        job.trigger_service_id.as_deref(),
                        name,
                    )
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "a env '{}' (job) aponta para o secret '{name}', que não pôde ser resolvido \
                             (procurado nos secrets do job '{}', do serviço gatilho e do projeto '{}'): {e}",
                            ev.key,
                            job.id,
                            job.project_id
                        )
                    })?
            }
        };
        env_map.insert(ev.key.clone(), value);
    }

    tracing::info!(
        job_id = %job.id,
        job_vars = job.env_vars.len(),
        total = env_map.len(),
        "resolve_env: vars resolvidas (projeto [+ serviço] + overrides do job)"
    );

    Ok(env_map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Vínculo no mesmo projeto: o `api` recebe `DB_HOST`/`DB_PORT`/`DB_URL` do `db` (Postgres em
    /// Compose), e a variável que a pessoa digitou ganha da injetada.
    #[tokio::test]
    async fn vinculo_entrega_host_porta_e_url_e_a_variavel_digitada_ganha() {
        use crate::domain::{ComposeSource, EnvVar, EnvVarValue, ServiceLink, ServiceSource};
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::connect(dir.path()).await.unwrap());
        db.seed_project_for_test("p1");
        db.seed_service_for_test("p1", "s_db");
        db.seed_service_for_test("p1", "s_api");
        let plain = |k: &str, v: &str| EnvVar {
            key: k.into(),
            value: EnvVarValue::Plain(v.into()),
        };
        db.infra()
            .update(|st| {
                for svc in &mut st.projects[0].services {
                    if svc.id == "s_db" {
                        svc.spec.name = "db".into();
                        svc.spec.port = 5432;
                        svc.spec.product = Some("postgres".into());
                        svc.spec.source = ServiceSource::Compose(ComposeSource {
                            content: "services:\n  rp_db:\n    image: postgres:18\n".into(),
                            ingress_service: None,
                        });
                        svc.spec.env_vars = vec![
                            plain("POSTGRES_USER", "app"),
                            plain("POSTGRES_PASSWORD", "segredo"),
                            plain("POSTGRES_DB", "loja"),
                        ];
                    } else {
                        svc.spec.name = "api".into();
                        svc.spec.links = vec![ServiceLink {
                            service_id: "s_db".into(),
                            env_prefix: None,
                            resource: None,
                        }];
                        svc.spec.env_vars = vec![plain("DB_PORT", "6543")];
                    }
                }
                Ok(())
            })
            .unwrap();
        let secrets = SecretsManager::new(db.clone()).unwrap();
        let api = crate::store::services::get(&db, "s_api")
            .await
            .unwrap()
            .unwrap();
        let map: HashMap<_, _> = resolve(&db, &secrets, &api)
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(map["DB_HOST"], "rp_db");
        assert_eq!(
            map["DB_PORT"], "6543",
            "a variável digitada ganha da injetada"
        );
        assert!(
            map["DB_URL"].starts_with("postgresql://app:segredo@rp_db:5432/loja"),
            "{}",
            map["DB_URL"]
        );

        // O provedor não recebe nada do consumidor, e um vínculo para um serviço apagado é pulado.
        let db_svc = crate::store::services::get(&db, "s_db")
            .await
            .unwrap()
            .unwrap();
        let own: HashMap<_, _> = resolve(&db, &secrets, &db_svc)
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert!(!own.contains_key("API_HOST"));
        // Fase 2: com um database de recurso, a URL aponta para ele e vem `DB_DATABASE`.
        let mut with_db = api.clone();
        with_db.spec.links[0].resource = Some(crate::domain::LinkResource {
            name: "pedidos".into(),
            public_read: false,
        });
        let res: HashMap<_, _> = link_env(&db, &secrets, &with_db)
            .await
            .into_iter()
            .collect();
        assert_eq!(res["DB_DATABASE"], "pedidos");
        assert!(
            res["DB_URL"].ends_with("@rp_db:5432/pedidos"),
            "{}",
            res["DB_URL"]
        );

        let mut orphan = api.clone();
        orphan.spec.links[0].service_id = "nao_existe".into();
        assert!(link_env(&db, &secrets, &orphan).await.is_empty());
    }

    #[tokio::test]
    async fn secret_ausente_vira_erro_que_diz_qual_e_onde() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::connect(dir.path()).await.unwrap());
        let secrets = SecretsManager::new(db).unwrap();

        let err = secret_value(
            &secrets,
            "proj_1",
            None,
            "DB_PASSWORD",
            "DATABASE_PASSWORD",
            "serviço",
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("DB_PASSWORD"), "{err}");
        assert!(err.contains("DATABASE_PASSWORD"), "{err}");
        assert!(err.contains("proj_1"), "{err}");
    }

    #[tokio::test]
    async fn secret_existente_continua_resolvendo() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::connect(dir.path()).await.unwrap());
        db.seed_project_for_test("proj_1");
        let secrets = SecretsManager::new(db).unwrap();
        secrets.set("proj_1", "TOKEN", "abc123").await.unwrap();

        let v = secret_value(&secrets, "proj_1", None, "TOKEN", "API_TOKEN", "projeto")
            .await
            .unwrap();
        assert_eq!(v, "abc123");
    }

    /// A env do serviço enxerga o secret do serviço antes do de projeto; o job com serviço gatilho
    /// enxerga os dois; o job autônomo só o do projeto.
    #[tokio::test]
    async fn escopos_de_secret_na_resolucao_de_servico_e_job() {
        use crate::domain::{EnvVar, EnvVarValue};
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::connect(dir.path()).await.unwrap());
        db.seed_project_for_test("p1");
        db.seed_service_for_test("p1", "s1");
        let secrets = SecretsManager::new(db.clone()).unwrap();
        secrets.set("p1", "SENHA", "do-projeto").await.unwrap();
        secrets.set("p1", "SO_PROJETO", "pp").await.unwrap();
        secrets
            .set_scoped("p1", Some("s1"), "SENHA", "do-servico")
            .await
            .unwrap();
        secrets
            .set_scoped("p1", Some("s1"), "SO_SERVICO", "ss")
            .await
            .unwrap();
        let sec = |k: &str, n: &str| EnvVar {
            key: k.into(),
            value: EnvVarValue::Secret(n.into()),
        };
        // env do serviço: SENHA (serviço vence), SO_PROJETO (cai no projeto), SO_SERVICO
        db.infra()
            .update(|st| {
                st.projects[0].services[0].spec.env_vars = vec![
                    sec("A", "SENHA"),
                    sec("B", "SO_PROJETO"),
                    sec("C", "SO_SERVICO"),
                ];
                Ok(())
            })
            .unwrap();
        let svc = crate::store::services::get(&db, "s1")
            .await
            .unwrap()
            .unwrap();
        let map: std::collections::HashMap<_, _> = resolve(&db, &secrets, &svc)
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(map["A"], "do-servico");
        assert_eq!(map["B"], "pp");
        assert_eq!(map["C"], "ss");

        // job COM gatilho: o override do job pode apontar para um secret do serviço gatilho
        let mk_job = |trigger: Option<&str>| crate::domain::Job {
            id: "j".into(),
            project_id: "p1".into(),
            trigger_service_id: trigger.map(String::from),
            name: "j".into(),
            compose: String::new(),
            git_source: None,
            main_service: "m".into(),
            env_vars: vec![sec("X", "SO_SERVICO")],
            env_comments: vec![],
            enabled: true,
            recurrence: None,
            last_run_at: None,
            next_run_at: None,
            created_at: chrono::Utc::now(),
        };
        let com: std::collections::HashMap<_, _> = resolve_job(&db, &secrets, &mk_job(Some("s1")))
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(com["X"], "ss");
        // job autônomo: o secret do serviço NÃO é visível → erro de deploy claro
        let e = resolve_job(&db, &secrets, &mk_job(None))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("SO_SERVICO") && e.contains("job"), "{e}");
    }
}
