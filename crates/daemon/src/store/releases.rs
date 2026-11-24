//! Tabela `release`: o que rodou em cada deploy que chegou a Live (imagem fixada + configuração
//! congelada). Histórico de execução, como `deployment`: **não** vai ao YAML de infra nem ao git.
//! Ver `docs/plano-releases-ambientes-rotas-volumes.md` §1.

use super::Db;
use crate::domain::{EnvVar, Release, ReleaseImageStatus, ServiceSpec};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ulid::Ulid;

type ReleaseRow = (
    String,         // id
    String,         // service_id
    String,         // deployment_id
    i64,            // number
    String,         // image
    Option<String>, // image_ref
    String,         // spec (JSON)
    String,         // project_env (JSON)
    Option<i64>,    // restored_from_number
    Option<String>, // promoted_from
    DateTime<Utc>,  // created_at
);

const SELECT_COLS: &str = "id, service_id, deployment_id, number, image, image_ref, spec, \
                           project_env, restored_from_number, promoted_from, created_at";

fn row_to_release(row: ReleaseRow) -> Result<Release> {
    let (
        id,
        service_id,
        deployment_id,
        number,
        image,
        image_ref,
        spec_json,
        project_env_json,
        restored_from_number,
        promoted_from,
        created_at,
    ) = row;
    let spec: ServiceSpec = serde_json::from_str(&spec_json)?;
    let project_env: Vec<EnvVar> = serde_json::from_str(&project_env_json).unwrap_or_default();
    Ok(Release {
        id,
        service_id,
        deployment_id,
        number: number as u32,
        image,
        image_ref,
        spec,
        project_env,
        restored_from_number: restored_from_number.map(|number| number as u32),
        promoted_from,
        created_at,
        is_current: false,
        image_status: ReleaseImageStatus::NotApplicable,
    })
}

/// O que gravar de uma release nova (o número é dado aqui, na mesma transação).
pub struct NewRelease<'a> {
    pub service_id: &'a str,
    pub deployment_id: &'a str,
    pub image: &'a str,
    pub image_ref: Option<&'a str>,
    pub spec: &'a ServiceSpec,
    pub project_env: &'a [EnvVar],
    pub restored_from_number: Option<u32>,
    pub promoted_from: Option<&'a str>,
}

/// Grava a release com o próximo número do serviço. Idempotente por deploy: se o deploy já virou
/// release (um `Live` regravado), devolve a que existe.
pub async fn create(db: &Db, new: NewRelease<'_>) -> Result<Release> {
    if let Some(existing) = get_by_deployment(db, new.deployment_id).await? {
        return Ok(existing);
    }
    let id = format!("rel_{}", Ulid::new());
    let spec_json = serde_json::to_string(new.spec)?;
    let project_env_json = serde_json::to_string(new.project_env)?;
    let mut transaction = db.pool().begin().await?;
    let (next_number,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(number), 0) + 1 FROM release WHERE service_id = ?")
            .bind(new.service_id)
            .fetch_one(&mut *transaction)
            .await?;
    sqlx::query(
        "INSERT INTO release (id, service_id, deployment_id, number, image, image_ref, spec,
                              project_env, restored_from_number, promoted_from, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(new.service_id)
    .bind(new.deployment_id)
    .bind(next_number)
    .bind(new.image)
    .bind(new.image_ref)
    .bind(&spec_json)
    .bind(&project_env_json)
    .bind(new.restored_from_number.map(i64::from))
    .bind(new.promoted_from)
    .bind(Utc::now())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    get(db, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("release not found after insert: {id}"))
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Release>> {
    let row =
        sqlx::query_as::<_, ReleaseRow>(&format!("SELECT {SELECT_COLS} FROM release WHERE id = ?"))
            .bind(id)
            .fetch_optional(db.pool())
            .await?;
    row.map(row_to_release).transpose()
}

pub async fn get_by_deployment(db: &Db, deployment_id: &str) -> Result<Option<Release>> {
    let row = sqlx::query_as::<_, ReleaseRow>(&format!(
        "SELECT {SELECT_COLS} FROM release WHERE deployment_id = ?"
    ))
    .bind(deployment_id)
    .fetch_optional(db.pool())
    .await?;
    row.map(row_to_release).transpose()
}

/// As releases do serviço, da mais nova para a mais velha.
pub async fn list_for_service(db: &Db, service_id: &str, limit: usize) -> Result<Vec<Release>> {
    let rows = sqlx::query_as::<_, ReleaseRow>(&format!(
        "SELECT {SELECT_COLS} FROM release WHERE service_id = ? ORDER BY number DESC LIMIT ?"
    ))
    .bind(service_id)
    .bind(limit as i64)
    .fetch_all(db.pool())
    .await?;
    rows.into_iter().map(row_to_release).collect()
}

/// As imagens que a limpeza do Docker não pode apagar: as das `per_service` releases mais novas de
/// cada serviço (com referência de imagem conhecida).
pub async fn protected_image_refs(db: &Db, per_service: usize) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT image_ref FROM (
             SELECT image_ref,
                    ROW_NUMBER() OVER (PARTITION BY service_id ORDER BY number DESC) AS position
             FROM release WHERE image_ref IS NOT NULL
         ) WHERE position <= ?",
    )
    .bind(per_service as i64)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.into_iter().map(|(image_ref,)| image_ref).collect())
}

/// Apaga o histórico de releases de um serviço (o serviço foi apagado).
pub async fn delete_for_service(db: &Db, service_id: &str) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM release WHERE service_id = ?")
        .bind(service_id)
        .execute(db.pool())
        .await?
        .rows_affected())
}

/// Marca um deployment como "voltar para a release X": o executor pula o build e usa a imagem dela.
pub async fn set_deployment_source(db: &Db, deployment_id: &str, release_id: &str) -> Result<()> {
    sqlx::query("UPDATE deployment SET from_release_id = ? WHERE id = ?")
        .bind(release_id)
        .bind(deployment_id)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// A release de que este deployment é um "voltar", se for.
pub async fn deployment_source(db: &Db, deployment_id: &str) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT from_release_id FROM deployment WHERE id = ?")
            .bind(deployment_id)
            .fetch_optional(db.pool())
            .await?;
    Ok(row.and_then(|(release_id,)| release_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        crate::features::wizard::app_spec("api".into(), "prj_1".into())
    }

    async fn new_release(
        db: &Db,
        service_id: &str,
        deployment_id: &str,
        image_ref: &str,
    ) -> Release {
        create(
            db,
            NewRelease {
                service_id,
                deployment_id,
                image: "app:latest",
                image_ref: Some(image_ref),
                spec: &spec(),
                project_env: &[],
                restored_from_number: None,
                promoted_from: None,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn numera_por_servico_e_e_idempotente_por_deploy() {
        let db = Db::memory_for_test().await;
        let first = new_release(&db, "svc_a", "dep_1", "sha256:1").await;
        let second = new_release(&db, "svc_a", "dep_2", "sha256:2").await;
        let other = new_release(&db, "svc_b", "dep_3", "sha256:3").await;
        let again = new_release(&db, "svc_a", "dep_2", "sha256:2").await;
        assert_eq!((first.number, second.number, other.number), (1, 2, 1));
        assert_eq!(again.id, second.id);
        let listed = list_for_service(&db, "svc_a", 10).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|release| release.number)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(listed[0].spec, spec());
    }

    #[tokio::test]
    async fn protege_so_as_mais_novas_de_cada_servico() {
        let db = Db::memory_for_test().await;
        for index in 1..=5 {
            new_release(
                &db,
                "svc_a",
                &format!("dep_a{index}"),
                &format!("sha256:a{index}"),
            )
            .await;
        }
        new_release(&db, "svc_b", "dep_b1", "sha256:b1").await;
        let mut protected = protected_image_refs(&db, 3).await.unwrap();
        protected.sort();
        assert_eq!(
            protected,
            vec!["sha256:a3", "sha256:a4", "sha256:a5", "sha256:b1"]
        );
    }
}
