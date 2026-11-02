//! Marcas de runtime da sincronização com o git (Fase 7): o que o `pull` deixou **pendente de
//! decisão humana**. Moram no SQL (runtime), nunca no YAML que vai ao git.
//!
//! - `state_git_removed`: algo que **sumiu** do estado por um commit externo. O daemon nunca apaga
//!   container nem dados por causa disso (R6.5): só registra, e a pessoa confirma ou esquece.
//! - `pending_deploy`: serviços cujo `spec` (ou env/secrets) mudou por um commit externo. O daemon só
//!   atualiza o *declarado*; o que está rodando segue como antes até o próximo deploy.

use super::Db;
use anyhow::Result;

/// Linha de `state_git_removed`: `(id, kind, name, project, sha, at, compose_project)`.
type RemovedRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// `serviço:<id>` ou `projeto:<id>`.
    pub id: String,
    pub kind: String,
    pub name: String,
    /// Nome do projeto (para serviços); vazio para projetos.
    pub project: String,
    pub sha: String,
    pub at: String,
    /// Stack Compose do serviço removido (só serviços Compose). Containers de uma stack não levam o
    /// label `rustploy.service_id`; é por este nome que se reconhece o órfão.
    pub compose_project: Option<String>,
}

pub async fn add_removed(db: &Db, r: &Removed) -> Result<()> {
    sqlx::query(
        "INSERT INTO state_git_removed (id, kind, name, project, sha, at, compose_project)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, project = excluded.project,
            sha = excluded.sha, at = excluded.at, compose_project = excluded.compose_project",
    )
    .bind(&r.id)
    .bind(&r.kind)
    .bind(&r.name)
    .bind(&r.project)
    .bind(&r.sha)
    .bind(&r.at)
    .bind(&r.compose_project)
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn list_removed(db: &Db) -> Result<Vec<Removed>> {
    let rows: Vec<RemovedRow> = sqlx::query_as(
        "SELECT id, kind, name, project, sha, at, compose_project FROM state_git_removed
             ORDER BY at, id",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, kind, name, project, sha, at, compose_project)| Removed {
                id,
                kind,
                name,
                project,
                sha,
                at,
                compose_project,
            },
        )
        .collect())
}

/// Tira as marcas de tudo o que **voltou** ao estado (um rollback no git que devolve o serviço):
/// o container nunca foi tocado, então ele simplesmente volta a ser do serviço.
pub async fn clear_returned(
    db: &Db,
    service_ids: &std::collections::BTreeSet<String>,
    project_ids: &std::collections::BTreeSet<String>,
) -> Result<usize> {
    let mut n = 0;
    for r in list_removed(db).await? {
        let back = match r.id.split_once(':') {
            Some(("serviço", id)) => service_ids.contains(id),
            Some(("projeto", id)) => project_ids.contains(id),
            _ => false,
        };
        if back && dismiss_removed(db, &r.id).await? {
            n += 1;
        }
    }
    Ok(n)
}

pub async fn dismiss_removed(db: &Db, id: &str) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM state_git_removed WHERE id = ?")
        .bind(id)
        .execute(db.pool())
        .await?
        .rows_affected()
        > 0)
}

pub async fn set_pending(db: &Db, service_id: &str, sha: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO pending_deploy (service_id, sha, at) VALUES (?, ?, ?)
         ON CONFLICT(service_id) DO UPDATE SET sha = excluded.sha, at = excluded.at",
    )
    .bind(service_id)
    .bind(sha)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn clear_pending(db: &Db, service_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM pending_deploy WHERE service_id = ?")
        .bind(service_id)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn list_pending(db: &Db) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT service_id FROM pending_deploy ORDER BY service_id")
            .fetch_all(db.pool())
            .await?,
    )
}
