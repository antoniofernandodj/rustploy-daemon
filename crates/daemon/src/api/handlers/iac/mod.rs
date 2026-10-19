//! `Command`s de IaC (manifesto e pacote de serviço): só roteia para `features::iac`.

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::ServiceExportPlan { service_id } => {
            crate::features::iac::service_export::plan(state, service_id).await
        }
        Command::ServiceExport {
            service_id,
            include_values,
            project_env_keys,
        } => {
            crate::features::iac::service_export::export(
                state,
                service_id,
                include_values,
                project_env_keys,
            )
            .await
        }
        Command::ServiceImport(req) => {
            crate::features::iac::service_import::handle(state, req).await
        }
        Command::ManifestApply {
            manifests,
            prune,
            deploy,
        } => crate::features::iac::manifest_apply::handle(state, manifests, prune, deploy).await,
        Command::ManifestExport { project_id } => {
            crate::features::iac::manifest_export::handle(state, project_id).await
        }
        Command::ManifestExportAll => {
            crate::features::iac::manifest_export_all::handle(state).await
        }
        Command::ManifestImport {
            yaml,
            dotenv,
            prune,
            deploy,
        } => {
            crate::features::iac::manifest_import::handle(state, yaml, dotenv, prune, deploy).await
        }
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
