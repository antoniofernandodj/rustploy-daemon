//! Handlers de services: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod get_webhook_url;
pub mod regenerate_webhook_token;
pub mod service_archive_upload;
pub mod service_connection_info;
pub mod service_get;
pub mod service_list;
pub mod service_reload;
pub mod wizard;

use crate::bootstrap::state::AppState;
use crate::protocol::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::ServiceCreate(spec) => {
            if let Err(resp) = prepare_links(&state, &spec, None).await {
                return resp;
            }
            crate::features::services::create::handle(state.services_ctx(), spec).await
        }
        Command::ServiceList { project_id } => service_list::handle(state, project_id).await,
        Command::ServiceConnectionInfo { service_id } => {
            service_connection_info::handle(state, service_id).await
        }
        Command::ServiceGet { id } => service_get::handle(state, id).await,
        Command::ServiceUpdate { id, spec } => {
            if let Err(resp) = prepare_links(&state, &spec, Some(&id)).await {
                return resp;
            }
            crate::features::services::update::handle(state.services_ctx(), id, spec).await
        }
        Command::ServiceVolumeAttach { service_id, volume } => {
            let response = match crate::features::services::volumes::attach(
                &state.db,
                &state.docker,
                &service_id,
                volume,
            )
            .await
            {
                Ok(service) => RpResponse::Service(Box::new(service)),
                Err(e) => RpResponse::err("InvalidInput", e.to_string()),
            };
            state.docker_cache.df.invalidate().await;
            response
        }
        Command::ServiceVolumeDetach {
            service_id,
            container_path,
        } => {
            let response = match crate::features::services::volumes::detach(
                &state.db,
                &service_id,
                &container_path,
            )
            .await
            {
                Ok(service) => RpResponse::Service(Box::new(service)),
                Err(e) => RpResponse::err("InvalidInput", e.to_string()),
            };
            state.docker_cache.df.invalidate().await;
            response
        }
        Command::ServiceDelete {
            id,
            delete_volumes,
            delete_external_volumes,
        } => {
            let response = crate::features::services::delete::handle(
                state.services_ctx(),
                id,
                crate::features::services::delete::VolumeChoice {
                    own: delete_volumes,
                    borrowed: delete_external_volumes,
                },
            )
            .await;
            // A lista de volumes (tela Docker) muda: o volume ficou órfão ou saiu.
            state.docker_cache.df.invalidate().await;
            response
        }
        Command::ServiceStop { service_id } => {
            crate::features::deploy::stop::handle(state.deploy_ctx(), service_id).await
        }
        Command::ServiceReload { service_id } => service_reload::handle(state, service_id).await,
        Command::GetWebhookUrl { service_id } => get_webhook_url::handle(state, service_id).await,
        Command::RegenerateWebhookToken { service_id } => {
            regenerate_webhook_token::handle(state, service_id).await
        }
        Command::WizardCatalog { search } => wizard::catalog(search).await,
        Command::WizardCreate(req) => wizard::create(state, req).await,
        // `snapshot` chama `dispatch` internamente (fan-out de DaemonStatus/
        // ProjectList/…), então boxamos o future para quebrar a recursão async.
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}

/// Antes de gravar um spec: valida os vínculos e concede ao projeto o acesso aos bancos
/// compartilhados de outros projetos que ele passa a usar (o mesmo `SharedAccessGrant` da tela do
/// banco, que liga a rede e recusa ciclo). Conceder antes de gravar: se a gravação falhar depois, o
/// acesso fica, o que é inofensivo; o contrário deixaria um vínculo sem rede.
async fn prepare_links(
    state: &AppState,
    spec: &crate::domain::ServiceSpec,
    self_id: Option<&str>,
) -> Result<(), RpResponse> {
    let plan = crate::features::services::links::check(&state.db, spec, self_id).await?;
    for server_id in plan.grant_servers {
        let granted = crate::api::handlers::databases::shared_access::grant(
            state.clone(),
            server_id,
            spec.project_id.clone(),
        )
        .await;
        if let RpResponse::Err { .. } = granted {
            return Err(granted);
        }
    }
    Ok(())
}
