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
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::ServiceCreate(spec) => {
            crate::features::services::create::handle(state, spec).await
        }
        Command::ServiceList { project_id } => service_list::handle(state, project_id).await,
        Command::ServiceConnectionInfo { service_id } => {
            service_connection_info::handle(state, service_id).await
        }
        Command::ServiceGet { id } => service_get::handle(state, id).await,
        Command::ServiceUpdate { id, spec } => {
            crate::features::services::update::handle(state, id, spec).await
        }
        Command::ServiceDelete { id } => crate::features::services::delete::handle(state, id).await,
        Command::ServiceStop { service_id } => {
            crate::features::deploy::stop::handle(state, service_id).await
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
