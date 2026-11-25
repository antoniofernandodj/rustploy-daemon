//! Releases fora do executor: a lista com o estado de cada uma (é a atual? a imagem ainda existe?)
//! e o diff. O "voltar" mora em `api/handlers/deploys/releases.rs`, que junta a gravação do spec
//! (serviços) com o deploy (este motor). Ver `docs/arquivo/plano-releases-ambientes-rotas-volumes.md` §1.

use crate::domain::{
    DeployState, Release, ReleaseDiff, ReleaseImageStatus, ReleaseSide, ServiceSource,
    diff_releases, image_ref_is_pullable,
};
use crate::engine::DockerClient;
use crate::store::{Db, releases};
use anyhow::{Result, anyhow};

/// Quantas releases a lista devolve, no máximo.
pub const RELEASE_LIST_LIMIT: usize = 50;

/// A disponibilidade da imagem de uma release.
pub async fn image_status(docker: &DockerClient, release: &Release) -> ReleaseImageStatus {
    if matches!(release.spec.source, ServiceSource::Compose(_)) {
        return ReleaseImageStatus::NotApplicable;
    }
    let Some(image_ref) = &release.image_ref else {
        return ReleaseImageStatus::Missing;
    };
    if docker.inner.inspect_image(image_ref).await.is_ok() {
        ReleaseImageStatus::Local
    } else if image_ref_is_pullable(image_ref) {
        ReleaseImageStatus::Pullable
    } else {
        ReleaseImageStatus::Missing
    }
}

/// As releases do serviço, da mais nova para a mais velha, com `is_current` e `image_status`.
pub async fn list(db: &Db, docker: &DockerClient, service_id: &str) -> Result<Vec<Release>> {
    let mut list = releases::list_for_service(db, service_id, RELEASE_LIST_LIMIT).await?;
    let live_deployment = crate::store::deployments::list_for_service(db, service_id, 50)
        .await?
        .into_iter()
        .find(|deployment| deployment.state == DeployState::Live)
        .map(|deployment| deployment.id);
    for release in &mut list {
        release.is_current = live_deployment.as_deref() == Some(release.deployment_id.as_str());
        release.image_status = image_status(docker, release).await;
    }
    Ok(list)
}

/// O que muda indo de `release_id` para `against_release_id` — ou, sem ele, indo do que está
/// **declarado agora** (spec do serviço + variáveis do projeto) para a release. É o que um "voltar
/// para esta" mudaria.
pub async fn diff(
    db: &Db,
    release_id: &str,
    against_release_id: Option<&str>,
) -> Result<ReleaseDiff> {
    let release = releases::get(db, release_id)
        .await?
        .ok_or_else(|| anyhow!("release not found: {release_id}"))?;
    let to = ReleaseSide {
        image_ref: release.image_ref.as_deref(),
        spec: &release.spec,
        project_env: &release.project_env,
    };
    let to_label = format!("v{}", release.number);
    match against_release_id {
        Some(other_id) => {
            let other = releases::get(db, other_id)
                .await?
                .ok_or_else(|| anyhow!("release not found: {other_id}"))?;
            if other.service_id != release.service_id {
                return Err(anyhow!("as duas releases são de serviços diferentes"));
            }
            let from = ReleaseSide {
                image_ref: other.image_ref.as_deref(),
                spec: &other.spec,
                project_env: &other.project_env,
            };
            Ok(ReleaseDiff {
                from_label: format!("v{}", other.number),
                to_label,
                changes: diff_releases(&from, &to),
            })
        }
        None => {
            let service = crate::store::services::get(db, &release.service_id)
                .await?
                .ok_or_else(|| anyhow!("service not found: {}", release.service_id))?;
            let project_env = crate::store::projects::get(db, &service.spec.project_id)
                .await?
                .map(|project| project.env_vars)
                .unwrap_or_default();
            let from = ReleaseSide {
                image_ref: None,
                spec: &service.spec,
                project_env: &project_env,
            };
            Ok(ReleaseDiff {
                from_label: "current".to_string(),
                to_label,
                changes: diff_releases(&from, &to),
            })
        }
    }
}

/// A release do deploy que está Live agora no serviço (`None` = nada no ar, ou o deploy no ar é de
/// antes das releases existirem).
pub async fn current_release(db: &Db, service_id: &str) -> Result<Option<Release>> {
    let live_deployment = crate::store::deployments::list_for_service(db, service_id, 50)
        .await?
        .into_iter()
        .find(|deployment| deployment.state == DeployState::Live);
    match live_deployment {
        Some(deployment) => releases::get_by_deployment(db, &deployment.id).await,
        None => Ok(None),
    }
}
