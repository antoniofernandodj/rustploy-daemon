//! Dump lógico de banco (Postgres, MySQL/MariaDB, MongoDB), feito **dentro do container do
//! próprio serviço**: as ferramentas já estão lá na versão certa e as credenciais são as env vars
//! que o container já tem — nenhum valor passa pelo daemon nem por quem chama.
//!
//! Redis, Kafka, RabbitMQ e os demais seguem como volume (tar).

use crate::features::databases::migration::{counts_plan, image_version, source_image};
use crate::features::databases::shared_db::{self, Engine};
use anyhow::{Result, anyhow, bail};
use shared::Service;
use std::path::Path;
use std::process::Stdio;

/// Um banco que sabemos despejar e restaurar.
#[derive(Debug, Clone, PartialEq)]
pub struct DbTarget {
    pub engine: Engine,
    /// Container do servidor (no ar).
    pub container: String,
    /// Nome do database (`*` = a instância inteira, só MongoDB).
    pub db: String,
}

/// O motor do serviço, se for um que despejamos (e não um servidor compartilhado).
pub fn engine_of(svc: &Service) -> Option<Engine> {
    if svc.spec.shared.is_some() {
        return None;
    }
    Engine::from_kind(svc.spec.product.as_deref())
}

/// Script `sh` que imprime o nome do database (lido das env vars do container).
fn db_name_script(e: Engine) -> &'static str {
    match e {
        Engine::Postgres => r#"echo "${POSTGRES_DB:-$POSTGRES_USER}""#,
        Engine::MySql | Engine::MariaDb => r#"echo "$MYSQL_DATABASE""#,
        Engine::Mongo => "echo '*'",
    }
}

/// `sh -c` que escreve o dump no stdout.
pub fn dump_script(e: Engine) -> &'static str {
    match e {
        Engine::Postgres => {
            r#"PGPASSWORD="$POSTGRES_PASSWORD" pg_dump -U "$POSTGRES_USER" -d "${POSTGRES_DB:-$POSTGRES_USER}" --format=custom --no-owner --no-acl"#
        }
        Engine::MySql => {
            r#"export MYSQL_PWD="$MYSQL_ROOT_PASSWORD"; exec mysqldump -uroot --single-transaction --routines --triggers --events --no-tablespaces --set-gtid-purged=OFF --add-drop-database --databases "$MYSQL_DATABASE""#
        }
        Engine::MariaDb => {
            r#"export MYSQL_PWD="$MYSQL_ROOT_PASSWORD"; B=$(command -v mariadb-dump || command -v mysqldump); exec "$B" -uroot --single-transaction --routines --triggers --events --add-drop-database --databases "$MYSQL_DATABASE""#
        }
        Engine::Mongo => {
            r#"exec mongodump --archive --quiet --username "$MONGO_INITDB_ROOT_USERNAME" --password "$MONGO_INITDB_ROOT_PASSWORD" --authenticationDatabase admin"#
        }
    }
}

/// `sh -c` que lê o dump do stdin e o aplica, substituindo o conteúdo do database.
pub fn restore_script(e: Engine) -> &'static str {
    match e {
        Engine::Postgres => {
            r#"PGPASSWORD="$POSTGRES_PASSWORD" exec pg_restore -U "$POSTGRES_USER" -d "${POSTGRES_DB:-$POSTGRES_USER}" --clean --if-exists --no-owner --no-acl --exit-on-error"#
        }
        // o dump já traz DROP DATABASE / CREATE DATABASE (`--add-drop-database`)
        Engine::MySql | Engine::MariaDb => {
            r#"B=$(command -v mariadb || command -v mysql); MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec "$B" -uroot"#
        }
        Engine::Mongo => {
            r#"exec mongorestore --archive --drop --quiet --username "$MONGO_INITDB_ROOT_USERNAME" --password "$MONGO_INITDB_ROOT_PASSWORD" --authenticationDatabase admin"#
        }
    }
}

/// Motor do container do serviço, ou a causa de não dar (serviço parado…).
pub async fn target(docker: &crate::engine::DockerClient, svc: &Service) -> Result<DbTarget> {
    let engine = engine_of(svc).ok_or_else(|| anyhow!("não é um banco que o Rustploy despeja"))?;
    let container = crate::features::deploy::shared_net::server_container(&docker.inner, svc)
        .await
        .ok_or_else(|| {
            anyhow!(
                "ligue o serviço: o dump lógico precisa do banco no ar (parado, só dá para \
                 copiar o volume, e isso fica para quando o serviço for um volume comum)"
            )
        })?;
    shared_db::wait_ready(engine, &container, 10).await?;
    let out = shared_db::exec_sh(engine, &container, db_name_script(engine), "").await?;
    let db = out.trim().to_string();
    if db.is_empty() || (db != "*" && !crate::features::databases::migration::valid_source_db(&db))
    {
        bail!("não consegui descobrir o nome do database deste serviço nas env vars dele");
    }
    Ok(DbTarget {
        engine,
        container,
        db,
    })
}

/// Despeja o database em `out`.
pub async fn dump_to(t: &DbTarget, out: &Path) -> Result<()> {
    let file = std::fs::File::create(out)?;
    let res = crate::engine::cli()
        .args([
            "exec",
            "-i",
            &t.container,
            "sh",
            "-c",
            dump_script(t.engine),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("falha ao executar o motor de containers: {e}"))?;
    if !res.status.success() {
        bail!(
            "o dump do banco falhou: {}",
            String::from_utf8_lossy(&res.stderr).trim()
        );
    }
    Ok(())
}

/// Restaura o dump `from` no database.
pub async fn restore_from(t: &DbTarget, from: &Path) -> Result<()> {
    let file = std::fs::File::open(from)?;
    let res = crate::engine::cli()
        .args([
            "exec",
            "-i",
            &t.container,
            "sh",
            "-c",
            restore_script(t.engine),
        ])
        .stdin(Stdio::from(file))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("falha ao executar o motor de containers: {e}"))?;
    if !res.status.success() {
        bail!(
            "restaurar o banco falhou: {}",
            String::from_utf8_lossy(&res.stderr).trim()
        );
    }
    Ok(())
}

/// `tabela|contagem` por linha; `None` para MongoDB (a instância inteira, sem conferência).
pub async fn counts(t: &DbTarget) -> Result<Option<String>> {
    if t.engine == Engine::Mongo {
        return Ok(None);
    }
    let (sh, stdin) = counts_plan(t.engine, &t.db);
    Ok(Some(
        shared_db::exec_sh(t.engine, &t.container, &sh, &stdin)
            .await?
            .trim()
            .to_string(),
    ))
}

/// Imagem do servidor (`postgres:18`), se o compose a declara.
pub fn version_of(svc: &Service) -> Option<String> {
    source_image(svc)
}

/// O destino aceita um dump vindo de `origem`? Motor igual e versão igual ou mais nova.
pub fn compatible(
    dest_engine: Engine,
    dest_image: Option<&str>,
    src_engine: &str,
    src_image: Option<&str>,
) -> Result<()> {
    if src_engine != dest_engine.kind_id() {
        bail!(
            "o pacote é de um banco `{src_engine}` e este serviço é `{}` (MySQL ↔ MariaDB também não)",
            dest_engine.kind_id()
        );
    }
    if let (Some(s), Some(d)) = (
        src_image.and_then(image_version),
        dest_image.and_then(image_version),
    ) && d < s
    {
        bail!(
            "o banco deste serviço ({}.{}) é mais antigo que o do pacote ({}.{})",
            d.0,
            d.1,
            s.0,
            s.1
        );
    }
    Ok(())
}

/// Diferenças entre as contagens `tabela|n` gravadas e as de agora (vazio = igual).
pub fn diff_counts(before: &str, after: &str) -> Vec<String> {
    let parse = |s: &str| -> std::collections::BTreeMap<String, String> {
        s.lines()
            .filter_map(|l| l.rsplit_once('|'))
            .map(|(t, n)| (t.trim().to_string(), n.trim().to_string()))
            .collect()
    };
    let (b, a) = (parse(before), parse(after));
    let mut out = Vec::new();
    for (t, n) in &b {
        match a.get(t) {
            None => out.push(format!("{t}: existia ({n} linhas) e sumiu")),
            Some(m) if m != n => out.push(format!("{t}: {n} → {m}")),
            _ => {}
        }
    }
    for t in a.keys().filter(|t| !b.contains_key(*t)) {
        out.push(format!("{t}: apareceu"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_usam_so_env_do_container() {
        for e in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Mongo,
        ] {
            for s in [dump_script(e), restore_script(e)] {
                assert!(s.contains('$'), "{e:?}: sem env var");
                // nada de senha literal nem de pipe (o exit code tem de ser o da ferramenta)
                assert!(!s.contains(" | "), "{e:?}: pipe esconde falha");
            }
        }
    }

    #[test]
    fn so_compativel_com_mesmo_motor_e_versao_igual_ou_nova() {
        assert!(
            compatible(
                Engine::Postgres,
                Some("postgres:18"),
                "postgres",
                Some("postgres:16")
            )
            .is_ok()
        );
        assert!(
            compatible(
                Engine::Postgres,
                Some("postgres:18"),
                "postgres",
                Some("postgres:18")
            )
            .is_ok()
        );
        assert!(
            compatible(
                Engine::Postgres,
                Some("postgres:15"),
                "postgres",
                Some("postgres:18")
            )
            .is_err()
        );
        assert!(
            compatible(
                Engine::MySql,
                Some("mysql:8"),
                "mariadb",
                Some("mariadb:11")
            )
            .is_err()
        );
        // tag ilegível: a checagem de versão é pulada
        assert!(
            compatible(
                Engine::Postgres,
                Some("postgres:latest"),
                "postgres",
                Some("postgres:18")
            )
            .is_ok()
        );
    }

    #[test]
    fn diferenca_de_contagens() {
        assert!(diff_counts("a|1\nb|2", "a|1\nb|2").is_empty());
        let d = diff_counts("a|1\nb|2", "a|1\nb|3\nc|0");
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d[0].contains("b: 2 → 3") && d[1].contains("c: apareceu"));
    }
}
