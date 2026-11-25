//! `Command::EnvironmentCreate`, `EnvironmentList`, `EnvironmentCompare`, `EnvironmentPromote` e
//! `EnvironmentDestroy`.
//! Ver `docs/arquivo/plano-releases-ambientes-rotas-volumes.md` §2.

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
    // Um serviço no ar desde antes das releases ganha agora a release do que está rodando.
    let release =
        match environments::ensure_baseline_release(&state.db, &state.docker, &from_service_id)
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

/// Destrói um ambiente: os serviços (parados e apagados, volumes próprios só com `delete_volumes`),
/// os jobs, as rotas e o projeto. Recusa a origem e um nome de confirmação errado. Se algo falhar
/// no meio, para e diz o quê: o que já saiu não volta, o resto fica (dá para repetir).
pub async fn destroy(
    state: AppState,
    project_id: String,
    confirm_name: String,
    delete_volumes: bool,
) -> RpResponse {
    let snapshot = state.db.infra().snapshot();
    let Some(project) = snapshot.projects.iter().find(|p| p.id == project_id) else {
        return RpResponse::err("NotFound", "project not found");
    };
    let Some(link) = project.environment.as_ref() else {
        return RpResponse::err(
            "InvalidInput",
            "este projeto não é um ambiente: só um ambiente pode ser destruído assim",
        );
    };
    if confirm_name.trim() != link.name {
        return RpResponse::err(
            "InvalidInput",
            format!("confirme digitando o nome do ambiente ({})", link.name),
        );
    }
    let service_ids = removal_order(&project.services);
    let job_ids: Vec<String> = project.jobs.iter().map(|j| j.id.clone()).collect();
    let route_ids: Vec<String> = project.routes.iter().map(|r| r.id.clone()).collect();
    let environment_name = link.name.clone();
    drop(snapshot);

    for service_id in service_ids {
        let response = crate::features::services::delete::handle(
            state.services_ctx(),
            service_id.clone(),
            crate::features::services::delete::VolumeChoice {
                own: delete_volumes,
                borrowed: false,
            },
        )
        .await;
        if let RpResponse::Err { .. } = response {
            return response;
        }
    }
    for job_id in job_ids {
        let response = crate::api::handlers::jobs::job_delete::handle(state.clone(), job_id).await;
        if let RpResponse::Err { .. } = response {
            return response;
        }
    }
    let had_routes = !route_ids.is_empty();
    for route_id in route_ids {
        if let Err(e) = crate::features::ingress::routes::delete(&state.db, &route_id) {
            return RpResponse::err("DatabaseError", e.to_string());
        }
    }
    if had_routes {
        crate::features::ingress::routes::sync(
            &state.db,
            &state.secrets,
            &state.ingress,
            &state.tls,
        );
    }
    let response = super::project_delete::handle(state, project_id.clone()).await;
    if matches!(response, RpResponse::Ok) {
        tracing::info!(project_id = %project_id, environment = %environment_name, delete_volumes,
            "ambiente destruído");
    }
    response
}

/// A ordem de apagar os serviços de um projeto: primeiro os que dependem (ou usam por vínculo), depois
/// os usados, porque apagar um serviço do qual outro depende é recusado. Num ciclo (que o daemon não
/// deixa criar), o resto vai na ordem em que está.
fn removal_order(services: &[crate::store::infra::model::ServiceInfra]) -> Vec<String> {
    let mut remaining: Vec<&crate::store::infra::model::ServiceInfra> = services.iter().collect();
    let mut order = Vec::new();
    while !remaining.is_empty() {
        let used = |id: &str, others: &[&crate::store::infra::model::ServiceInfra]| {
            others.iter().any(|other| {
                other.id != id
                    && (other.spec.depends_on.iter().any(|d| d == id)
                        || other.spec.links.iter().any(|l| l.service_id == id))
            })
        };
        let position = remaining
            .iter()
            .position(|service| !used(&service.id, &remaining))
            .unwrap_or(0);
        order.push(remaining.remove(position).id.clone());
    }
    order
}
