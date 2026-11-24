//! Tabela `orphan_volume`: os volumes que ficaram para trás quando um serviço foi apagado sem
//! "apagar também os volumes", e de quem eram. É o que deixa a tela de volumes dizer "órfão do
//! serviço `db` do projeto `loja`" em vez de um nome solto. Ver
//! `docs/plano-releases-ambientes-rotas-volumes.md` §4.

use super::Db;
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct OrphanVolume {
    pub service_name: String,
    pub project_name: String,
    pub deleted_at: DateTime<Utc>,
}

pub async fn record(db: &Db, name: &str, service_name: &str, project_name: &str) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO orphan_volume (name, service_name, project_name, deleted_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(name)
    .bind(service_name)
    .bind(project_name)
    .bind(Utc::now())
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn forget(db: &Db, name: &str) -> Result<()> {
    sqlx::query("DELETE FROM orphan_volume WHERE name = ?")
        .bind(name)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn all(db: &Db) -> Result<HashMap<String, OrphanVolume>> {
    let rows: Vec<(String, String, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT name, service_name, project_name, deleted_at FROM orphan_volume")
            .fetch_all(db.pool())
            .await?;
    Ok(rows
        .into_iter()
        .map(|(name, service_name, project_name, deleted_at)| {
            (
                name,
                OrphanVolume {
                    service_name,
                    project_name,
                    deleted_at,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grava_le_e_esquece() {
        let db = Db::memory_for_test().await;
        record(&db, "rp_x_pgdata", "db", "loja").await.unwrap();
        record(&db, "rp_x_pgdata", "db", "loja").await.unwrap();
        let orphans = all(&db).await.unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans["rp_x_pgdata"].service_name, "db");
        forget(&db, "rp_x_pgdata").await.unwrap();
        assert!(all(&db).await.unwrap().is_empty());
    }
}
