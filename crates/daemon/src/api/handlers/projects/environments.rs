//! `Command::EnvironmentCreate`, `EnvironmentList`, `EnvironmentCompare` e `EnvironmentPromote`.
//! Ver `docs/plano-releases-ambientes-rotas-volumes.md` §2.

use crate::bootstrap::state::AppState;
use crate::domain::{ServiceSource, ServiceStatus};
use crate::features::environments;
use crate::protocol::Response as RpResponse;

pub async fn create(state: AppState, project_id: String, name: String) -> RpResponse {
    match environments::create(&state.db, &state.db_path, &project_id, &name).await {
        Ok(project) => RpResponse::Project(Box::new(project)),
        Err(e) => RpResponse::err("InvalidInput", e.to_string()),
    }
}

pub fn list(state: AppState, project_id: String) -> RpResponse {
    RpResponse::Environments(environments::list(&state.db, &project_id))
}

pub async fn compare(state: AppState, project_id: String) -> RpResponse {
    match environments::compare(&state.db, &project_id).await {
        Ok(comparison) => RpResponse::EnvironmentComparison(Box::new(comparison)),
        Err(e) => RpResponse::err("InvalidInput", e.to_string()),
    }
}

/// Promove a release no ar de um serviço para o irmão no outro ambiente: deploy da mesma imagem,
/// sem build e sem mudar a configuração do destino.
pub async fn promote(
    state: AppState,
    from_service_id: String,
    to_service_id: String,
) -> RpResponse {
    if !environments::are_siblings(&state.db, &from_service_id, &to_service_id) {
        return RpResponse::err(
            "InvalidInput",
            "os dois serviços não são irmãos (um não é a cópia do outro num ambiente)",
        );
    }
    let target = match crate::store::services::get(&state.db, &to_service_id).await {
        Ok(Some(service)) => service,
        Ok(None) => return RpResponse::err("NotFound", "service not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    if matches!(target.spec.source, ServiceSource::Compose(_)) {
        return RpResponse::err(
            "InvalidInput",
            "serviço Compose não é promovido: não há uma imagem só para levar",
        );
    }
    if matches!(
        target.status,
        ServiceStatus::Deploying | ServiceStatus::Queued
    ) {
        return RpResponse::err(
            "ServiceAlreadyDeploying",
            "deploy already in progress or queued",
        );
    }
    let release = match crate::features::deploy::release::current_release(
        &state.db,
        &from_service_id,
    )
    .await
    {
        Ok(Some(release)) => release,
        Ok(None) => {
            return RpResponse::err(
                "NotFound",
                "o serviço de origem não tem uma release no ar para promover",
            );
        }
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    if release.image_ref.is_none() || matches!(release.spec.source, ServiceSource::Compose(_)) {
        return RpResponse::err(
            "InvalidInput",
            "a release no ar não tem uma imagem fixada para promover",
        );
    }
    if crate::features::deploy::release::image_status(&state.docker, &release).await
        == crate::domain::ReleaseImageStatus::Missing
    {
        return RpResponse::err(
            "ReleaseImageMissing",
            "a imagem da release no ar não existe mais neste servidor",
        );
    }
    crate::features::deploy::start::handle_from_release(
        state.deploy_ctx(),
        to_service_id,
        release.id,
    )
    .await
}
