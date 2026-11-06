//! A migração **única** SQL → YAML (Fase 4 do plano, seção R4).
//!
//! Passos de [`run`]: (1) cópia de segurança do SQLite; (2) lê as tabelas de infra; (3) grava o
//! YAML numa transação do repositório; (4) **ida e volta**: relê do disco e compara com o que
//! veio do SQL — divergiu, desfaz tudo e devolve erro; (5) grava a marca de "migrado".
//! As tabelas antigas **não são apagadas** (regra "comentar, não remover"): são o rollback.
//!
//! Roda no `store::connect`, uma vez: depois dela os módulos de `store/` leem e escrevem no YAML.

// As linhas do SQL são lidas como tuplas, como no resto de `store/` (uma struct por tabela só para
// ser descartada logo em seguida seria ruído).
#![allow(clippy::type_complexity)]

use super::model::*;
use super::txn::{FileTree, Tx};
use crate::store::{Db, daemon_settings};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use shared::{EnvComment, EnvVar, JobGitSource, Recurrence, ServiceSpec};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Chave em `daemon_settings` que marca a migração como concluída.
pub const MARK_KEY: &str = "infra_yaml_migrated_at";

#[derive(Debug)]
pub enum Outcome {
    /// Já tinha sido feita antes; nada a fazer.
    AlreadyDone,
    Migrated {
        backup: Option<PathBuf>,
        /// O que ficou de fora por referência quebrada (órfãos no SQL antigo).
        warnings: Vec<String>,
        projects: usize,
        services: usize,
    },
}

type Dt = DateTime<Utc>;

/// Lê as tabelas de infra e monta o [`InfraState`] + avisos de órfãos.
pub async fn read_sql(db: &Db) -> Result<(InfraState, Vec<String>)> {
    let mut warnings = vec![];

    // --- settings ---------------------------------------------------------------------------
    let docker_cleanup_config = daemon_settings::sql_get(db, "docker_cleanup_config")
        .await?
        .map(|raw| {
            serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::Value::String(raw.clone()))
        });
    let settings = SettingsInfra {
        acme_email: daemon_settings::sql_get(db, "acme_email").await?,
        registry_domain: daemon_settings::sql_get(db, "registry_domain").await?,
        docker_cleanup_config,
    };

    // --- providers --------------------------------------------------------------------------
    let rows: Vec<(
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    )> = sqlx::query_as(
        "SELECT id, kind, name, base_url, auth_mode, oauth_client_id, oauth_client_secret_enc, \
         account_login, account_avatar, created_at FROM git_provider",
    )
    .fetch_all(db.pool())
    .await?;
    let providers = rows
        .into_iter()
        .map(|r| {
            Ok(ProviderInfra {
                id: r.0,
                kind: r.1,
                name: r.2,
                base_url: r.3,
                auth_mode: r.4,
                oauth_client_id: r.5,
                oauth_client_secret_enc: r.6,
                account_login: r.7,
                account_avatar: r.8,
                created_at: DateTime::parse_from_rfc3339(&r.9)?.with_timezone(&Utc),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // --- projetos ---------------------------------------------------------------------------
    let rows: Vec<(
        String,
        String,
        Option<String>,
        String,
        String,
        Option<String>,
        Dt,
    )> = sqlx::query_as(
        "SELECT id, name, description, env_vars, env_comments, network_name, created_at \
             FROM project",
    )
    .fetch_all(db.pool())
    .await?;
    let mut projects: BTreeMap<String, ProjectInfra> = BTreeMap::new();
    for (id, name, description, env_vars, env_comments, network_name, created_at) in rows {
        let env_vars: Vec<EnvVar> =
            serde_json::from_str(&env_vars).with_context(|| format!("env do projeto {name}"))?;
        let env_comments: Vec<EnvComment> = serde_json::from_str(&env_comments)
            .with_context(|| format!("comentários do projeto {name}"))?;
        projects.insert(
            id.clone(),
            ProjectInfra {
                id,
                name,
                description,
                env_vars,
                env_comments,
                network_name,
                created_at,
                services: vec![],
                jobs: vec![],
                secrets: vec![],
                managed_databases: vec![],
                shared_access: vec![],
            },
        );
    }

    // --- serviços (+ webhook) ---------------------------------------------------------------
    let tokens: BTreeMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT service_id, token FROM webhook_token")
            .fetch_all(db.pool())
            .await?
            .into_iter()
            .collect();
    let rows: Vec<(String, String, String, String, Option<String>, Dt)> = sqlx::query_as(
        "SELECT id, name, project_id, spec, compose_project, created_at FROM service",
    )
    .fetch_all(db.pool())
    .await?;
    let mut service_owner: BTreeMap<String, String> = BTreeMap::new(); // service_id → project_id
    for (id, name, project_id, spec, compose_project, created_at) in rows {
        let Some(p) = projects.get_mut(&project_id) else {
            warnings.push(format!(
                "serviço '{name}' ({id}) aponta para o projeto {project_id}, que não existe: fora do YAML"
            ));
            continue;
        };
        // SQLite antigo: o JSON ainda pode trazer `db_kind` (hoje `product`); sem renomear, o
        // `serde(default)` o engoliria e o banco viraria serviço comum.
        let mut spec_json: serde_json::Value = serde_json::from_str(&spec)
            .with_context(|| format!("spec do serviço '{name}' ({id})"))?;
        if let Some(obj) = spec_json.as_object_mut()
            && let Some(v) = obj.remove("db_kind")
        {
            obj.entry("product").or_insert(v);
        }
        let spec: ServiceSpec = serde_json::from_value(spec_json)
            .with_context(|| format!("spec do serviço '{name}' ({id})"))?;
        service_owner.insert(id.clone(), project_id);
        p.services.push(ServiceInfra {
            webhook_token: tokens.get(&id).cloned(),
            id,
            spec,
            compose_project,
            created_at,
            secrets: vec![],
        });
    }

    // Tokens de webhook de serviços que não existem mais (o daemon antigo não os limpava ao
    // apagar o serviço): não entram no YAML, mas são avisados.
    for id in tokens.keys() {
        if !service_owner.contains_key(id) {
            warnings.push(format!(
                "token de webhook do serviço {id}, que não existe: fora do YAML"
            ));
        }
    }

    // --- jobs -------------------------------------------------------------------------------
    let rows: Vec<(
        String,
        String,
        String,
        String,
        String,
        String,
        bool,
        Option<String>,
        Dt,
        Option<String>,
        String,
        String,
    )> = sqlx::query_as(
        "SELECT id, project_id, trigger_service_id, name, compose, main_service, enabled, \
         recurrence, created_at, git_source, env_vars, env_comments FROM job",
    )
    .fetch_all(db.pool())
    .await?;
    for (
        id,
        project_id,
        trigger,
        name,
        compose,
        main_service,
        enabled,
        recurrence,
        created_at,
        git_source,
        env_vars,
        env_comments,
    ) in rows
    {
        let Some(p) = projects.get_mut(&project_id) else {
            warnings.push(format!(
                "job '{name}' ({id}) aponta para o projeto {project_id}, que não existe: fora do YAML"
            ));
            continue;
        };
        // No SQL a coluna é NOT NULL: a string vazia significa "job autônomo".
        let trigger_service_id = (!trigger.is_empty()).then_some(trigger);
        if let Some(t) = &trigger_service_id
            && !service_owner.contains_key(t)
        {
            warnings.push(format!(
                "job '{name}' ({id}) tem o serviço gatilho {t}, que não existe: fora do YAML"
            ));
            continue;
        }
        let recurrence: Option<Recurrence> = recurrence
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let git_source: Option<JobGitSource> = git_source
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        p.jobs.push(JobInfra {
            id,
            trigger_service_id,
            name,
            compose,
            git_source,
            main_service,
            env_vars: serde_json::from_str(&env_vars)?,
            env_comments: serde_json::from_str(&env_comments)?,
            enabled,
            recurrence,
            created_at,
        });
    }

    // --- secrets ----------------------------------------------------------------------------
    let rows: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT id, project_id, key, value FROM secret")
            .fetch_all(db.pool())
            .await?;
    for (id, project_id, key, value) in rows {
        match projects.get_mut(&project_id) {
            Some(p) => p.secrets.push(SecretInfra { id, key, value }),
            None => warnings.push(format!(
                "secret '{key}' ({id}) aponta para o projeto {project_id}, que não existe: fora do YAML"
            )),
        }
    }

    // --- bancos gerenciados -----------------------------------------------------------------
    let rows: Vec<(
        String,
        String,
        String,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Dt,
    )> = sqlx::query_as(
        "SELECT id, server_service_id, project_id, name, password, env_var, connection_limit, \
         statement_timeout_ms, created_at FROM managed_database",
    )
    .fetch_all(db.pool())
    .await?;
    for r in rows {
        let (id, server, project_id, name) = (r.0, r.1, r.2, r.3);
        let Some(p) = projects.get_mut(&project_id) else {
            warnings.push(format!(
                "banco '{name}' ({id}) aponta para o projeto {project_id}, que não existe: fora do YAML"
            ));
            continue;
        };
        if !service_owner.contains_key(&server) {
            warnings.push(format!(
                "banco '{name}' ({id}) aponta para o servidor {server}, que não existe: fora do YAML"
            ));
            continue;
        }
        p.managed_databases.push(ManagedDatabaseInfra {
            id,
            server_service_id: server,
            name,
            password: r.4,
            env_var: r.5,
            connection_limit: r.6.map(|v| v as u32),
            statement_timeout_ms: r.7.map(|v| v as u32),
            created_at: r.8,
        });
    }

    // --- acessos a banco compartilhado ------------------------------------------------------
    let rows: Vec<(String, String, Dt)> =
        sqlx::query_as("SELECT server_service_id, project_id, created_at FROM shared_access")
            .fetch_all(db.pool())
            .await?;
    for (server, project_id, created_at) in rows {
        let Some(p) = projects.get_mut(&project_id) else {
            warnings.push(format!(
                "acesso ao servidor {server} do projeto {project_id}, que não existe: fora do YAML"
            ));
            continue;
        };
        if !service_owner.contains_key(&server) {
            warnings.push(format!(
                "acesso do projeto '{}' ao servidor {server}, que não existe: fora do YAML",
                p.name
            ));
            continue;
        }
        p.shared_access.push(SharedAccessInfra {
            server_service_id: server,
            created_at,
        });
    }

    let mut state = InfraState {
        settings,
        providers,
        projects: projects.into_values().collect(),
    };
    state.normalize();
    Ok((state, warnings))
}

/// Cópia consistente do SQLite (`VACUUM INTO`, que é segura com o banco em uso). Nunca
/// sobrescreve uma cópia anterior: se `<db>.pre-yaml.bak` existe, usa `.1`, `.2`…
async fn backup_sqlite(db: &Db, db_path: &Path) -> Result<PathBuf> {
    let base = db_path.as_os_str().to_owned();
    let mut n = 0u32;
    let dest = loop {
        let mut p = base.clone();
        p.push(".pre-yaml.bak");
        if n > 0 {
            p.push(format!(".{n}"));
        }
        let p = PathBuf::from(p);
        if !p.exists() {
            break p;
        }
        n += 1;
    };
    let quoted = dest.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{quoted}'"))
        .execute(db.pool())
        .await
        .with_context(|| format!("copiando o SQLite para {}", dest.display()))?;
    restrict(&dest);
    Ok(dest)
}

#[cfg(unix)]
fn restrict(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict(_: &Path) {}

/// Apaga tudo o que a migração gravou (volta ao "sem infra/").
fn undo(tree: &FileTree) -> Result<()> {
    let mut tx = Tx::new();
    for rel in tree.list()? {
        tx.delete(rel);
    }
    if tx.is_empty() {
        return Ok(());
    }
    tree.commit(tx)
}

/// Roda a migração. `db_path` = caminho do arquivo SQLite, para a cópia de segurança (`None` em
/// teste com banco em memória). Idempotente: depois de feita, devolve [`Outcome::AlreadyDone`].
pub async fn run(db: &Db, tree: &FileTree, db_path: Option<&Path>) -> Result<Outcome> {
    if daemon_settings::sql_get(db, MARK_KEY).await?.is_some() {
        return Ok(Outcome::AlreadyDone);
    }

    // Instalação nova (nada de infra no SQL) não precisa de cópia de segurança.
    let has_data: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM project) + (SELECT COUNT(*) FROM git_provider)",
    )
    .fetch_one(db.pool())
    .await?;
    let backup = match db_path {
        Some(p) if has_data > 0 => Some(backup_sqlite(db, p).await?),
        _ => None,
    };

    let (state, warnings) = read_sql(db).await?;
    state
        .validate()
        .context("o estado lido do SQL é inconsistente")?;

    if let Err(e) = state.write(tree) {
        let _ = undo(tree);
        return Err(e.context("gravando o YAML"));
    }

    // Ida e volta: o que está no disco tem de ser exatamente o que veio do SQL.
    let check = (|| -> Result<()> {
        let back = InfraState::read(tree)?.context("o YAML gravado não pode ser relido")?;
        if back != state {
            bail!("{}", first_difference(&state, &back));
        }
        Ok(())
    })();
    if let Err(e) = check {
        let _ = undo(tree);
        return Err(e.context(
            "verificação de ida e volta falhou; o YAML foi removido e o SQL continua valendo",
        ));
    }

    daemon_settings::set(db, MARK_KEY, &Utc::now().to_rfc3339()).await?;
    Ok(Outcome::Migrated {
        backup,
        warnings,
        projects: state.projects.len(),
        services: state.projects.iter().map(|p| p.services.len()).sum(),
    })
}

/// Mensagem útil quando a ida e volta diverge: diz **onde**, sem despejar o estado inteiro
/// (que pode ter segredos).
fn first_difference(a: &InfraState, b: &InfraState) -> String {
    if a.settings != b.settings {
        return "divergência na configuração (settings.yml)".into();
    }
    if a.providers != b.providers {
        return "divergência nos git providers (providers.yml)".into();
    }
    let ids = |s: &InfraState| {
        s.projects
            .iter()
            .map(|p| p.id.clone())
            .collect::<BTreeSet<_>>()
    };
    if ids(a) != ids(b) {
        return "divergência na lista de projetos".into();
    }
    for pa in &a.projects {
        if let Some(pb) = b.projects.iter().find(|p| p.id == pa.id)
            && pa != pb
        {
            return format!("divergência no projeto '{}' ({})", pa.name, pa.id);
        }
    }
    "divergência desconhecida".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_db() -> Db {
        Db::memory_for_test().await
    }

    async fn exec(db: &Db, sql: &str) {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }

    fn spec_json(name: &str, project_id: &str) -> String {
        let spec = crate::features::wizard::app_spec(name.to_string(), project_id.to_string());
        serde_json::to_string(&spec).unwrap()
    }

    /// Um banco com um pouco de tudo, inclusive sujeira (órfãos).
    async fn seed(db: &Db) {
        let now = "2026-01-02T03:04:05.123456789+00:00";
        exec(db, &format!(
            "INSERT INTO project (id, name, description, env_vars, env_comments, network_name, created_at) \
             VALUES ('p1', 'App Um', 'desc', '[{{\"key\":\"A\",\"value\":{{\"Plain\":\"1\"}}}}]', '[]', 'rp_p1', '{now}'), \
                    ('p2', 'Outro', NULL, '[]', '[]', NULL, '{now}')")).await;
        exec(db, &format!(
            "INSERT INTO service (id, name, project_id, spec, status, created_at, updated_at, compose_project) \
             VALUES ('s1', 'api', 'p1', '{}', 'Running', '{now}', '{now}', NULL), \
                    ('s2', 'db', 'p1', '{}', 'Stopped', '{now}', '{now}', 'rp_s2_db'), \
                    ('s3', 'orfao', 'sumiu', '{}', 'Stopped', '{now}', '{now}', NULL)",
            spec_json("api", "p1"), spec_json("db", "p1"), spec_json("orfao", "sumiu"))).await;
        exec(
            db,
            &format!("INSERT INTO webhook_token VALUES ('s1', 'tok', '{now}'), ('apagado', 'velho', '{now}')"),
        )
        .await;
        exec(
            db,
            "INSERT INTO secret VALUES ('k1', 'p1', 'SENHA', 'cifrado')",
        )
        .await;
        exec(db, &format!(
            "INSERT INTO job (id, project_id, trigger_service_id, name, compose, main_service, enabled, \
             recurrence, created_at, env_vars, env_comments) \
             VALUES ('j1', 'p1', 's1', 'backup', 'x', 'main', 1, NULL, '{now}', '[]', '[]'), \
                    ('j2', 'p1', '', 'solto', 'x', 'main', 0, NULL, '{now}', '[]', '[]'), \
                    ('j3', 'p1', 'fantasma', 'quebrado', 'x', 'main', 1, NULL, '{now}', '[]', '[]')")).await;
        exec(db, &format!(
            "INSERT INTO managed_database VALUES ('m1', 's2', 'p2', 'loja', 'senha-em-claro', 'DATABASE_URL', 5, NULL, '{now}')")).await;
        exec(
            db,
            &format!("INSERT INTO shared_access VALUES ('s2', 'p2', '{now}')"),
        )
        .await;
        exec(db, &format!(
            "INSERT INTO git_provider (id, kind, name, base_url, auth_mode, oauth_client_id, \
             oauth_client_secret_enc, access_token_enc, refresh_token_enc, account_login, created_at) \
             VALUES ('g1', 'GitHub', 'gh', 'https://github.com', 'OAuth', 'cid', 'segredo-cifrado', 'TOKEN-DE-LOGIN', 'REFRESH', 'eu', '{now}')")).await;
        exec(
            db,
            "INSERT INTO daemon_settings VALUES ('acme_email', 'a@b.c'), \
             ('docker_cleanup_config', '{\"enabled\":true}'), ('docker_cleanup_last_run', 'x'), \
             ('webhook_base_url', 'velho')",
        )
        .await;
    }

    #[tokio::test]
    async fn migra_com_ida_e_volta_e_e_idempotente() {
        let db = mem_db().await;
        seed(&db).await;
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();

        let out = run(&db, &tree, None).await.unwrap();
        let Outcome::Migrated {
            warnings,
            projects,
            services,
            ..
        } = out
        else {
            panic!("devia migrar")
        };
        assert_eq!((projects, services), (2, 2));
        // órfãos: serviço s3, job j3, token de webhook de serviço apagado
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("webhook")));

        let st = InfraState::read(&tree).unwrap().unwrap();
        assert_eq!(st.settings.acme_email.as_deref(), Some("a@b.c"));
        assert_eq!(st.projects[0].services.len(), 2);
        assert_eq!(st.projects[0].jobs.len(), 2);
        assert!(
            st.projects[0]
                .jobs
                .iter()
                .any(|j| j.trigger_service_id.is_none())
        );
        assert_eq!(
            st.projects[1].managed_databases[0].connection_limit,
            Some(5)
        );

        // não vaza token de login nem chave descartada
        let tudo: String = tree
            .list()
            .unwrap()
            .iter()
            .map(|r| String::from_utf8(tree.read(r).unwrap().unwrap()).unwrap())
            .collect();
        assert!(!tudo.contains("TOKEN-DE-LOGIN") && !tudo.contains("REFRESH"));
        assert!(!tudo.contains("velho") && !tudo.contains("last_run"));
        assert!(tudo.contains("segredo-cifrado"));

        // segunda vez: nada a fazer
        assert!(matches!(
            run(&db, &tree, None).await.unwrap(),
            Outcome::AlreadyDone
        ));
    }

    #[tokio::test]
    async fn banco_vazio_migra_para_um_estado_vazio_valido() {
        let db = mem_db().await;
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();
        assert!(matches!(
            run(&db, &tree, None).await.unwrap(),
            Outcome::Migrated { .. }
        ));
        let st = InfraState::read(&tree).unwrap().unwrap();
        assert!(st.projects.is_empty());
    }

    #[tokio::test]
    async fn spec_ilegivel_aborta_sem_gravar_nada_nem_marcar() {
        let db = mem_db().await;
        seed(&db).await;
        exec(
            &db,
            "UPDATE service SET spec = 'isto não é json' WHERE id = 's1'",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();
        let e = run(&db, &tree, None).await.unwrap_err();
        assert!(format!("{e:#}").contains("api"), "{e:#}");
        assert!(InfraState::read(&tree).unwrap().is_none());
        assert!(daemon_settings::get(&db, MARK_KEY).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn faz_copia_do_sqlite_sem_sobrescrever_a_anterior() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let db = crate::store::connect(&data).await.unwrap();
        let path = data.join("rustploy.db");
        seed(&db).await;
        // `connect` já migrou o banco vazio e marcou; desfaz a marca para migrar o que foi semeado.
        daemon_settings::delete(&db, MARK_KEY).await.unwrap();
        let tree = FileTree::open(dir.path().join("infra")).unwrap();
        let Outcome::Migrated { backup, .. } = run(&db, &tree, Some(&path)).await.unwrap() else {
            panic!()
        };
        let b1 = backup.unwrap();
        assert!(b1.exists() && b1.to_string_lossy().ends_with(".pre-yaml.bak"));
        // a cópia tem os dados
        let copia = sqlx::SqlitePool::connect(&format!("sqlite:{}", b1.display()))
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM service")
            .fetch_one(&copia)
            .await
            .unwrap();
        assert_eq!(n, 3);
        // uma segunda migração (marca apagada) não pisa na cópia
        daemon_settings::delete(&db, MARK_KEY).await.unwrap();
        let Outcome::Migrated { backup, .. } = run(&db, &tree, Some(&path)).await.unwrap() else {
            panic!()
        };
        assert!(
            backup
                .unwrap()
                .to_string_lossy()
                .ends_with(".pre-yaml.bak.1")
        );
    }

    #[tokio::test]
    async fn boot_migra_banco_antigo_e_os_stores_leem_do_yaml() {
        use crate::store::{projects, services};
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        // 1º boot "da versão antiga": só cria o schema; depois semeia como se fosse o SQL de antes.
        {
            let db = crate::store::connect(&data).await.unwrap();
            seed(&db).await;
            daemon_settings::delete(&db, MARK_KEY).await.unwrap();
            // o YAML do boot vazio é descartado, como se `infra/` nunca tivesse existido
            undo(db.infra().tree()).unwrap();
        }
        // boot da versão nova
        let db = crate::store::connect(&data).await.unwrap();
        assert!(data.join("rustploy.db.pre-yaml.bak").exists());
        let ps = projects::list(&db).await.unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].name, "App Um");
        assert_eq!(services::list(&db, "p1").await.unwrap().len(), 2);
        assert_eq!(
            crate::store::git_providers::list(&db).await.unwrap()[0]
                .access_token_enc
                .as_deref(),
            Some("TOKEN-DE-LOGIN"),
            "os tokens de login seguem no SQL e voltam na leitura"
        );
        assert_eq!(
            daemon_settings::get(&db, "acme_email")
                .await
                .unwrap()
                .as_deref(),
            Some("a@b.c")
        );
        // o runtime foi semeado a partir do SQL antigo (s1 estava Running → intenção Running)
        assert_eq!(
            services::get(&db, "s1").await.unwrap().unwrap().status,
            shared::ServiceStatus::Running
        );
        assert_eq!(
            services::desired_state(&db, "s1").await.unwrap(),
            services::DesiredState::Running
        );
    }

    #[tokio::test]
    async fn infra_que_sumiu_depois_de_migrada_impede_o_boot() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        drop(crate::store::connect(&data).await.unwrap());
        std::fs::remove_dir_all(data.join("infra")).unwrap();
        let e = crate::store::connect(&data)
            .await
            .err()
            .expect("devia recusar");
        assert!(e.to_string().contains("não tem estado"), "{e}");
    }

    /// Migra uma **cópia** de um `rustploy.db` real (nunca o original). Uso:
    /// `RUSTPLOY_PROD_DB=/caminho/rustploy.db cargo test -p rustploy banco_de_producao -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "precisa de RUSTPLOY_PROD_DB"]
    async fn banco_de_producao() {
        use crate::store::{
            git_providers, job, managed_database, projects, services, shared_access,
        };
        let src =
            PathBuf::from(std::env::var("RUSTPLOY_PROD_DB").expect("defina RUSTPLOY_PROD_DB"));
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::copy(&src, data.join("rustploy.db")).unwrap();

        // contagens do SQL ANTES (o `connect` já migra)
        let pool =
            sqlx::SqlitePool::connect(&format!("sqlite:{}", data.join("rustploy.db").display()))
                .await
                .unwrap();
        let count = |t: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {t}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap() as usize
            }
        };
        let c_wh_orfaos: usize = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM webhook_token WHERE service_id NOT IN (SELECT id FROM service)",
        )
        .fetch_one(&pool)
        .await
        .unwrap() as usize;
        let (c_proj, c_svc, c_job, c_sec, c_mdb, c_acc, c_prov, c_wh) = (
            count("project").await,
            count("service").await,
            count("job").await,
            count("secret").await,
            count("managed_database").await,
            count("shared_access").await,
            count("git_provider").await,
            count("webhook_token").await - c_wh_orfaos,
        );
        println!("webhooks órfãos no SQL (serviço apagado): {c_wh_orfaos}");
        let tokens: Vec<String> = sqlx::query_scalar(
            "SELECT access_token_enc FROM git_provider WHERE access_token_enc IS NOT NULL
             UNION SELECT refresh_token_enc FROM git_provider WHERE refresh_token_enc IS NOT NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        pool.close().await;

        // o boot da versão nova
        let db = crate::store::connect(&data)
            .await
            .expect("a migração deve funcionar");
        let st = db.infra().snapshot();
        let n = |f: &dyn Fn(&ProjectInfra) -> usize| st.projects.iter().map(f).sum::<usize>();
        println!(
            "SQL  : projetos={c_proj} serviços={c_svc} jobs={c_job} secrets={c_sec} bancos={c_mdb} acessos={c_acc} providers={c_prov} webhooks={c_wh}"
        );
        println!(
            "YAML : projetos={} serviços={} jobs={} secrets={} bancos={} acessos={} providers={} webhooks={}",
            st.projects.len(),
            n(&|p| p.services.len()),
            n(&|p| p.jobs.len()),
            n(&|p| p.secrets.len()),
            n(&|p| p.managed_databases.len()),
            n(&|p| p.shared_access.len()),
            st.providers.len(),
            n(&|p| p
                .services
                .iter()
                .filter(|s| s.webhook_token.is_some())
                .count()),
        );
        assert_eq!(st.projects.len(), c_proj);
        assert_eq!(n(&|p| p.services.len()), c_svc, "serviços");
        assert_eq!(n(&|p| p.jobs.len()), c_job, "jobs");
        assert_eq!(n(&|p| p.secrets.len()), c_sec, "secrets");
        assert_eq!(n(&|p| p.managed_databases.len()), c_mdb, "bancos");
        assert_eq!(n(&|p| p.shared_access.len()), c_acc, "acessos");
        assert_eq!(st.providers.len(), c_prov, "providers");
        assert_eq!(
            n(&|p| p
                .services
                .iter()
                .filter(|s| s.webhook_token.is_some())
                .count()),
            c_wh,
            "webhooks"
        );

        // pelos stores (o caminho que os handlers usam)
        assert_eq!(projects::list(&db).await.unwrap().len(), c_proj);
        assert_eq!(services::list_all(&db).await.unwrap().len(), c_svc);
        assert_eq!(job::list_all(&db).await.unwrap().len(), c_job);
        assert_eq!(git_providers::list(&db).await.unwrap().len(), c_prov);
        let _ = (
            managed_database::get(&db, "x").await,
            shared_access::all(&db).await.unwrap(),
        );
        // runtime preservado: quem estava Running continua
        let running = services::get_running(&db).await.unwrap().len();
        let desired = services::get_desired_running(&db).await.unwrap().len();
        println!("runtime: Running={running} desired=Running={desired}");

        // vazamento: tokens de login do provider não podem estar em nenhum arquivo
        let tree = db.infra().tree();
        let tudo: String = tree
            .list()
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&tree.read(r).unwrap().unwrap()).into_owned())
            .collect();
        for t in &tokens {
            assert!(
                !tudo.contains(t.as_str()),
                "token de login vazou para o YAML"
            );
        }
        let bytes = tudo.len();
        println!("arquivos={} bytes={bytes}", tree.list().unwrap().len());
        println!(
            "backup existe: {}",
            data.join("rustploy.db.pre-yaml.bak").exists()
        );
        let achados = crate::store::infra::audit::plaintext_findings(&db.infra().snapshot());
        println!("auditoria de valores em claro: {} achados", achados.len());
        for f in &achados {
            println!(
                "  {:?} | {} | {} | {}",
                f.certainty, f.project, f.scope, f.what
            );
        }

        // reabrir: não migra de novo e o estado é o mesmo
        let antes = (*st).clone();
        drop(st);
        drop(db);
        let db2 = crate::store::connect(&data).await.unwrap();
        assert_eq!(*db2.infra().snapshot(), antes, "reabrir mudou o estado");
        println!("PRODUÇÃO OK");
    }
}
