//! `Command::ReleaseList`, `ReleaseDiff` e `ReleaseRollback`. Ver
//! `docs/plano-releases-ambientes-rotas-volumes.md` §1.

use crate::bootstrap::state::AppState;
use crate::domain::{ReleaseImageStatus, ServiceStatus};
use crate::features::deploy::release;
use crate::protocol::Response as RpResponse;
use tracing::info;

pub async fn list(state: AppState, service_id: String) -> RpResponse {
    match release::list(&state.db, &state.docker, &service_id).await {
        Ok(releases) => RpResponse::Releases(releases),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}

pub async fn diff(
    state: AppState,
    release_id: String,
    against_release_id: Option<String>,
) -> RpResponse {
    match release::diff(&state.db, &release_id, against_release_id.as_deref()).await {
        Ok(diff) => RpResponse::ReleaseDiff(Box::new(diff)),
        Err(e) => RpResponse::err("NotFound", e.to_string()),
    }
}

/// Voltar para uma release: grava o spec dela no serviço (pela mesma validação de um
/// `ServiceUpdate`) e dispara um deploy que pula o build e usa a imagem dela.
pub async fn rollback(state: AppState, release_id: String) -> RpResponse {
    let target = match crate::store::releases::get(&state.db, &release_id).await {
        Ok(Some(release)) => release,
        Ok(None) => return RpResponse::err("NotFound", "release not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    let service = match crate::store::services::get(&state.db, &target.service_id).await {
        Ok(Some(service)) => service,
        Ok(None) => return RpResponse::err("NotFound", "service not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    // Antes de gravar qualquer coisa: com o deploy em curso, o spec mudaria e o deploy não sairia.
    if matches!(
        service.status,
        ServiceStatus::Deploying | ServiceStatus::Queued
    ) {
        return RpResponse::err(
            "ServiceAlreadyDeploying",
            "deploy already in progress or queued",
        );
    }
    if release::image_status(&state.docker, &target).await == ReleaseImageStatus::Missing {
        return RpResponse::err(
            "ReleaseImageMissing",
            format!(
                "a imagem da v{} não existe mais neste servidor e não há de onde baixá-la",
                target.number
            ),
        );
    }

    // O nome e o projeto continuam os de agora: o nome entra no nome dos containers e da stack, e
    // um serviço renomeado depois da release não deve voltar a se chamar como antes.
    let mut spec = target.spec.clone();
    spec.name = service.spec.name.clone();
    spec.project_id = service.spec.project_id.clone();

    info!(
        release_id = %target.id,
        service_id = %service.id,
        number = target.number,
        "release_rollback: gravando o spec da release e disparando o deploy"
    );
    match crate::features::services::update::handle(state.services_ctx(), service.id.clone(), spec)
        .await
    {
        RpResponse::Service(_) => {}
        RpResponse::Err { code, message } => {
            return RpResponse::err(
                code,
                format!(
                    "a configuração da v{} não foi aceita: {message}",
                    target.number
                ),
            );
        }
        other => return other,
    }
    crate::features::deploy::start::handle_from_release(state.deploy_ctx(), service.id, target.id)
        .await
}
