//! `Command`s de IaC (manifesto e pacote de serviço): só roteia para `features::iac`.

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::ServiceExportPlan { service_id } => {
            crate::features::iac::service_export::plan(state.iac_ctx(), service_id).await
        }
        Command::ServiceExport {
            service_id,
            include_values,
            project_env_keys,
        } => {
            crate::features::iac::service_export::export(
                state.iac_ctx(),
                service_id,
                include_values,
                project_env_keys,
            )
            .await
        }
        Command::ServiceImport(req) => {
            crate::features::iac::service_import::handle(state.iac_ctx(), req).await
        }
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
