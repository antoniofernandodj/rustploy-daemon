//! Conexão SQLite (`Db`) e as migrações do schema, feitas à mão com
//! `add_column_if_missing`.

pub mod build_logs;
pub mod daemon_settings;
pub mod deployments;
pub mod git_providers;
pub mod infra;
pub mod job;
pub mod job_log;
pub mod job_run;
pub mod log_sink;
pub mod managed_database;
pub mod migration;
pub mod projects;
pub mod registry;
pub mod registry_tokens;
pub mod services;
pub mod shared_access;
pub mod state_git_marks;
pub mod webhook_tokens;

use anyhow::Result;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};
use std::str::FromStr;
use tracing::{info, warn};

/// O armazenamento do daemon: o **SQLite** (runtime: deploys, logs, status…) e o **repositório de
/// infra** em YAML (projetos, serviços, jobs…; `store::infra`). Plano de migração:
/// `docs/plano-backup-estado-em-git.md` (Fase 4d). `Db` faz `Deref` para o pool, então
/// `db.begin()` & cia. seguem valendo; para passar como executor do `sqlx` use [`Db::pool`].
#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    infra: std::sync::Arc<infra::repo::Repo>,
}

impl Db {
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// O repositório de infra (leituras: `snapshot()`; escritas: `update()`).
    pub fn infra(&self) -> &infra::repo::Repo {
        &self.infra
    }

    /// Cria (se ainda não existe) um projeto com este id — para testes que precisam de um dono
    /// para os serviços sem passar por `projects::create` (que gera o id sozinho).
    #[cfg(test)]
    pub(crate) fn seed_project_for_test(&self, id: &str) {
        self.infra()
            .update(|st| {
                if !st.projects.iter().any(|p| p.id == id) {
                    st.projects.push(infra::model::ProjectInfra {
                        id: id.into(),
                        name: id.into(),
                        description: None,
                        env_vars: vec![],
                        env_comments: vec![],
                        network_name: None,
                        created_at: chrono::Utc::now(),
                        services: vec![],
                        jobs: vec![],
                        secrets: vec![],
                        managed_databases: vec![],
                        shared_access: vec![],
                        service_order: vec![],
                    });
                }
                Ok(())
            })
            .unwrap();
    }

    /// Cria (se ainda não existe) um serviço com este id dentro do projeto (que já deve existir).
    #[cfg(test)]
    pub(crate) fn seed_service_for_test(&self, project_id: &str, service_id: &str) {
        self.infra()
            .update(|st| {
                let p = st
                    .projects
                    .iter_mut()
                    .find(|p| p.id == project_id)
                    .expect("projeto de teste não existe");
                if !p.services.iter().any(|s| s.id == service_id) {
                    p.services.push(infra::model::ServiceInfra {
                        id: service_id.into(),
                        spec: crate::features::wizard::app_spec(
                            service_id.into(),
                            project_id.into(),
                        ),
                        compose_project: None,
                        created_at: chrono::Utc::now(),
                        webhook_token: None,
                        secrets: vec![],
                        backup: None,
                    });
                }
                Ok(())
            })
            .unwrap();
    }

    /// Banco que nunca chega a conectar (só para quem precisa de um `Db` de enfeite).
    #[cfg(test)]
    pub(crate) fn lazy_for_test() -> Db {
        Db {
            pool: SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
            infra: std::sync::Arc::new(infra::repo::Repo::temp()),
        }
    }

    /// Banco em memória + repositório em pasta temporária, para os testes.
    #[cfg(test)]
    pub(crate) async fn memory_for_test() -> Db {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        migrate(&pool).await.unwrap();
        Db {
            pool,
            infra: std::sync::Arc::new(infra::repo::Repo::temp()),
        }
    }
}

impl std::ops::Deref for Db {
    type Target = SqlitePool;
    fn deref(&self) -> &SqlitePool {
        &self.pool
    }
}

pub async fn connect(db_path: &std::path::Path) -> Result<Db> {
    std::fs::create_dir_all(db_path)?;
    let db_file = db_path.join("rustploy.db");
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_file.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePool::connect_with(opts).await?;
    migrate(&pool).await?;
    // O YAML de infra mora ao lado do banco (`<db_path>/infra`). Na primeira subida depois da
    // atualização, os dados de infra que estavam no SQL são migrados para lá (uma vez só).
    let infra = std::sync::Arc::new(infra::repo::Repo::open(db_path.join("infra"))?);
    let db = Db { pool, infra };
    match infra::migrate::run(&db, db.infra().tree(), Some(&db_file)).await? {
        infra::migrate::Outcome::AlreadyDone => {
            // Marcado como migrado, mas sem estado em disco: alguém apagou/moveu `infra/`. Subir
            // assim mostraria um servidor vazio e a próxima escrita o tornaria o novo normal.
            if !db.infra().is_initialized()? {
                anyhow::bail!(
                    "a infra já foi migrada para YAML, mas {} não tem estado. Restaure essa pasta \
                     (ou, para refazer a migração a partir do SQL antigo, apague a chave \
                     '{}' de daemon_settings).",
                    db_path.join("infra").display(),
                    infra::migrate::MARK_KEY
                );
            }
        }
        infra::migrate::Outcome::Migrated {
            backup,
            warnings,
            projects,
            services,
        } => {
            db.infra().reload()?;
            info!(projects, services, backup = ?backup, "infra migrada do SQL para YAML");
            for w in &warnings {
                warn!("migração SQL→YAML: {w}");
            }
        }
    }
    Ok(db)
}

/// Cria as tabelas e aplica as migrações incrementais (`ALTER TABLE`
/// idempotente via `add_column_if_missing`); roda a cada boot.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "
        CREATE TABLE IF NOT EXISTS project (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            description TEXT,
            env_vars    TEXT NOT NULL DEFAULT '[]',
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS service (
            id                 TEXT PRIMARY KEY,
            name               TEXT NOT NULL,
            project_id         TEXT NOT NULL,
            spec               TEXT NOT NULL,
            status             TEXT NOT NULL DEFAULT 'Stopped',
            live_container_id  TEXT,
            created_at         TEXT NOT NULL,
            updated_at         TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_service_project ON service(project_id);

        CREATE TABLE IF NOT EXISTS deployment (
            id          TEXT PRIMARY KEY,
            service_id  TEXT NOT NULL,
            image       TEXT NOT NULL,
            state       TEXT NOT NULL DEFAULT 'Pending',
            states_log  TEXT NOT NULL DEFAULT '[]',
            started_at  TEXT NOT NULL,
            finished_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_deployment_service  ON deployment(service_id);
        CREATE INDEX IF NOT EXISTS idx_deployment_started  ON deployment(started_at);

        CREATE TABLE IF NOT EXISTS secret (
            id         TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            UNIQUE(project_id, key)
        );

        CREATE TABLE IF NOT EXISTS build_log (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            deployment_id TEXT NOT NULL,
            line          TEXT NOT NULL,
            ts            TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_build_log_dep ON build_log(deployment_id);

        CREATE TABLE IF NOT EXISTS tls_cert (
            id         TEXT PRIMARY KEY,
            domain     TEXT NOT NULL UNIQUE,
            cert_pem   TEXT NOT NULL,
            key_pem    TEXT NOT NULL,
            expires_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS webhook_token (
            service_id TEXT PRIMARY KEY,
            token      TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS daemon_settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS git_provider (
            id                      TEXT PRIMARY KEY,
            kind                    TEXT NOT NULL,
            name                    TEXT NOT NULL,
            base_url                TEXT NOT NULL,
            auth_mode               TEXT NOT NULL,
            oauth_client_id         TEXT,
            oauth_client_secret_enc TEXT,
            access_token_enc        TEXT,
            refresh_token_enc       TEXT,
            account_login           TEXT,
            account_avatar          TEXT,
            created_at              TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS job (
            id                 TEXT PRIMARY KEY,
            project_id         TEXT NOT NULL,
            trigger_service_id TEXT NOT NULL,
            name               TEXT NOT NULL,
            compose            TEXT NOT NULL,
            main_service       TEXT NOT NULL,
            enabled            INTEGER NOT NULL DEFAULT 1,
            recurrence         TEXT,
            last_run_at        TEXT,
            next_run_at        TEXT,
            created_at         TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_job_project  ON job(project_id);
        CREATE INDEX IF NOT EXISTS idx_job_next_run ON job(next_run_at);

        CREATE TABLE IF NOT EXISTS job_run (
            id          TEXT PRIMARY KEY,
            job_id      TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT,
            exit_code   INTEGER,
            success     INTEGER
        );

        CREATE INDEX IF NOT EXISTS idx_job_run_job ON job_run(job_id);

        CREATE TABLE IF NOT EXISTS job_log (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            job_run_id TEXT NOT NULL,
            stream     TEXT NOT NULL DEFAULT 'Stdout',
            line       TEXT NOT NULL,
            ts         TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_job_log_run ON job_log(job_run_id);

        CREATE TABLE IF NOT EXISTS registry_repos (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS registry_blobs (
            digest     TEXT PRIMARY KEY,
            size       INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );

        -- PK composta (repo_id, digest): o MESMO conteúdo (mesmo digest) pode
        -- legitimamente pertencer a repos diferentes (duas tags/repos apontando
        -- pra imagem idêntica produzem manifests byte-a-byte iguais). Com
        -- `digest` como PK sozinho, o segundo repo a fazer push do mesmo
        -- conteúdo 'roubava' a posse do primeiro (ON CONFLICT sobrescrevia
        -- repo_id), quebrando list_repos (tamanho zerado), delete_manifest
        -- (404) e até o `docker pull` real do repo que perdia a posse.
        CREATE TABLE IF NOT EXISTS registry_manifests (
            repo_id    TEXT NOT NULL,
            digest     TEXT NOT NULL,
            media_type TEXT NOT NULL,
            size       INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (repo_id, digest)
        );

        CREATE INDEX IF NOT EXISTS idx_registry_manifests_digest ON registry_manifests(digest);

        CREATE TABLE IF NOT EXISTS registry_tags (
            repo_id         TEXT NOT NULL,
            tag             TEXT NOT NULL,
            manifest_digest TEXT NOT NULL,
            updated_at      TEXT NOT NULL,
            PRIMARY KEY (repo_id, tag)
        );

        CREATE TABLE IF NOT EXISTS registry_manifest_refs (
            manifest_digest TEXT NOT NULL,
            blob_digest     TEXT NOT NULL,
            PRIMARY KEY (manifest_digest, blob_digest)
        );

        CREATE TABLE IF NOT EXISTS registry_tokens (
            id           TEXT PRIMARY KEY,
            name         TEXT NOT NULL UNIQUE,
            token_sha256 TEXT NOT NULL,
            scope        TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            last_used_at TEXT
        );

        -- Projetos autorizados a alcançar um servidor de banco compartilhado
        -- (ver docs/plano-banco-compartilhado.md §2.3).
        -- Databases/usuários criados dentro de um servidor compartilhado.
        CREATE TABLE IF NOT EXISTS managed_database (
            id                   TEXT PRIMARY KEY,
            server_service_id    TEXT NOT NULL,
            project_id           TEXT NOT NULL,
            name                 TEXT NOT NULL,
            password             TEXT NOT NULL,
            env_var              TEXT NOT NULL,
            connection_limit     INTEGER,
            statement_timeout_ms INTEGER,
            created_at           TEXT NOT NULL,
            UNIQUE (server_service_id, name)
        );

        -- Assistente de migração para banco compartilhado.
        CREATE TABLE IF NOT EXISTS migration (
            id                TEXT PRIMARY KEY,
            project_id        TEXT NOT NULL,
            source_service_id TEXT NOT NULL,
            source_database   TEXT NOT NULL,
            dest_database_id  TEXT NOT NULL,
            env_var           TEXT NOT NULL,
            status            TEXT NOT NULL,
            steps             TEXT NOT NULL,
            log               TEXT NOT NULL DEFAULT '[]',
            env_changes       TEXT NOT NULL DEFAULT '[]',
            stopped_services  TEXT NOT NULL DEFAULT '[]',
            created_at        TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS shared_access (
            server_service_id TEXT NOT NULL,
            project_id        TEXT NOT NULL,
            created_at        TEXT NOT NULL,
            PRIMARY KEY (server_service_id, project_id)
        );
        ",
    )
    .execute(pool)
    .await?;

    // Migrações incrementais (SQLite não tem `ADD COLUMN IF NOT EXISTS`): rode
    // o ALTER e ignore o erro "duplicate column name" quando já foi aplicado.
    add_column_if_missing(
        pool,
        "ALTER TABLE project ADD COLUMN env_comments TEXT NOT NULL DEFAULT '[]'",
    )
    .await?;
    add_column_if_missing(pool, "ALTER TABLE job ADD COLUMN git_source TEXT").await?;
    add_column_if_missing(
        pool,
        "ALTER TABLE job ADD COLUMN env_vars TEXT NOT NULL DEFAULT '[]'",
    )
    .await?;
    add_column_if_missing(
        pool,
        "ALTER TABLE job ADD COLUMN env_comments TEXT NOT NULL DEFAULT '[]'",
    )
    .await?;

    // Nome da rede Docker do projeto e da stack Compose do serviço: gravados,
    // não derivados (ver `docs/plano-nome-gravado-rede-e-stack.md`).
    add_column_if_missing(pool, "ALTER TABLE project ADD COLUMN network_name TEXT").await?;
    add_column_if_missing(pool, "ALTER TABLE service ADD COLUMN compose_project TEXT").await?;

    // Fase 4d do plano `docs/plano-backup-estado-em-git.md`: a config do git provider vai para o
    // YAML; os tokens de login ficam aqui (nunca vão ao git). Preenche a partir da tabela antiga.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS git_provider_token (
            provider_id       TEXT PRIMARY KEY,
            access_token_enc  TEXT,
            refresh_token_enc TEXT
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO git_provider_token (provider_id, access_token_enc, refresh_token_enc)
         SELECT id, access_token_enc, refresh_token_enc FROM git_provider
         WHERE access_token_enc IS NOT NULL OR refresh_token_enc IS NOT NULL",
    )
    .execute(pool)
    .await?;

    // Fase 7: marcas do `pull` do git que esperam uma decisão humana (ver `store::state_git_marks`).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS state_git_removed (
            id      TEXT PRIMARY KEY,
            kind    TEXT NOT NULL,
            name    TEXT NOT NULL,
            project TEXT NOT NULL DEFAULT '',
            sha     TEXT NOT NULL,
            at      TEXT NOT NULL,
            compose_project TEXT
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pending_deploy (
            service_id TEXT PRIMARY KEY,
            sha        TEXT NOT NULL,
            at         TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    // Fase 5: quem revelou qual secret e quando (nunca o valor).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS secret_audit (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            at         TEXT NOT NULL,
            action     TEXT NOT NULL,
            project_id TEXT NOT NULL,
            service_id TEXT,
            name       TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    // Fase 4d: o que muda a cada execução do job sai da definição (que vai para o YAML).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS job_runtime (
            job_id      TEXT PRIMARY KEY,
            last_run_at TEXT,
            next_run_at TEXT
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO job_runtime (job_id, last_run_at, next_run_at)
         SELECT id, last_run_at, next_run_at FROM job
         WHERE NOT EXISTS (SELECT 1 FROM daemon_settings WHERE key = 'infra_yaml_migrated_at')",
    )
    .execute(pool)
    .await?;

    // Fase 2 do plano `docs/plano-backup-estado-em-git.md`: o runtime do serviço (status,
    // container live, intenção) sai da linha declarada para `service_runtime`. As colunas legadas
    // de `service` continuam existindo e sendo escritas (rollback do binário), mas a fonte da
    // verdade passa a ser esta tabela.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS service_runtime (
            service_id        TEXT PRIMARY KEY,
            status            TEXT NOT NULL DEFAULT 'Stopped',
            live_container_id TEXT,
            desired_state     TEXT NOT NULL DEFAULT 'Stopped',
            updated_at        TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    // Backfill (idempotente: só serviços ainda sem linha). Quem estava no ar ou a caminho
    // continua desejado no ar, para um reboot logo após a atualização religar tudo.
    let seeded = sqlx::query(
        "INSERT OR IGNORE INTO service_runtime (service_id, status, live_container_id, desired_state, updated_at)
         SELECT id, status, live_container_id,
                CASE WHEN status IN ('Running', 'Degraded', 'Deploying', 'Queued', 'Stopping')
                     THEN 'Running' ELSE 'Stopped' END,
                updated_at
         FROM service
         -- Depois da migração para YAML a tabela `service` está congelada: não ressuscitar
         -- runtime de serviço que já foi apagado.
         WHERE NOT EXISTS (SELECT 1 FROM daemon_settings WHERE key = 'infra_yaml_migrated_at')",
    )
    .execute(pool)
    .await?
    .rows_affected();
    if seeded > 0 {
        info!(
            services = seeded,
            "db::migrate: service_runtime preenchida a partir de service"
        );
    }

    // Preenche a rede dos projetos que ainda não têm com o nome que **já usam**
    // (fórmula legada: `rp_net_` + 8 primeiros chars do ID) — nenhuma rede
    // existente muda de nome. `instr` é 0 sem `_`, e `substr(id, 1, 8)` cobre.
    let filled = sqlx::query(
        "UPDATE project SET network_name = 'rp_net_' || substr(id, instr(id, '_') + 1, 8)
         WHERE network_name IS NULL",
    )
    .execute(pool)
    .await?
    .rows_affected();
    if filled > 0 {
        info!(
            projects = filled,
            "db::migrate: network_name preenchido com o nome legado"
        );
    }
    ensure_unique_index(
        pool,
        "idx_project_network_name",
        "project(network_name)",
        "dois projetos compartilham a mesma rede Docker (IDs com os mesmos 8 primeiros \
         caracteres); o daemon segue como antes, mas separá-los exige redeploy de um deles",
    )
    .await;

    Ok(())
}

/// Reexecuta a migração num banco já aberto (um boot seguinte), para os testes.
#[cfg(test)]
pub(crate) async fn connect_existing_for_test(db: &Db) {
    migrate(db.pool()).await.unwrap();
}

/// Cria um índice `UNIQUE` sem derrubar o boot: se já há valores repetidos (uma
/// colisão anterior ao plano de nome gravado), só loga — o daemon funciona como
/// antes e o operador resolve na mão, porque separar mexe em containers vivos.
/// O SQLite ignora `NULL` num índice único.
pub(crate) async fn ensure_unique_index(pool: &SqlitePool, name: &str, target: &str, why: &str) {
    let sql = format!("CREATE UNIQUE INDEX IF NOT EXISTS {name} ON {target}");
    if let Err(e) = sqlx::query(&sql).execute(pool).await {
        warn!(index = name, error = %e, "db::migrate: índice UNIQUE não criado: {why}");
    }
}

/// Executa um `ALTER TABLE ... ADD COLUMN`, tratando como no-op se a coluna já
/// existe (SQLite responde "duplicate column name").
async fn add_column_if_missing(pool: &SqlitePool, sql: &str) -> Result<()> {
    match sqlx::query(sql).execute(pool).await {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(e.into()),
    }
}
