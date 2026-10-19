//! Handlers de deploys: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod deploy_abort;
pub mod deploy_delete;
pub mod deploy_engine_status;
pub mod deploy_history;
pub mod deploy_queue_pause;
pub mod deploy_queue_promote;
pub mod deploy_queue_reorder;
pub mod deploy_rollback;
pub mod get_build_logs;
pub mod recent_deployments;
pub mod reconcile;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::DeployEngineStatus => deploy_engine_status::handle(state).await,
        Command::DeployStart { service_id } => {
            crate::features::deploy::start::handle(state, service_id).await
        }
        Command::DeployAbort { deployment_id } => deploy_abort::handle(state, deployment_id).await,
        Command::DeployQueuePromote { deployment_id } => {
            deploy_queue_promote::handle(state, deployment_id).await
        }
        Command::DeployQueueReorder { order } => deploy_queue_reorder::handle(state, order).await,
        Command::DeployQueuePause { paused } => deploy_queue_pause::handle(state, paused).await,
        Command::DeployDelete { deployment_id } => {
            deploy_delete::handle(state, deployment_id).await
        }
        Command::DeployHistory { service_id, limit } => {
            deploy_history::handle(state, service_id, limit).await
        }
        Command::DeployRollback { service_id } => deploy_rollback::handle(state, service_id).await,
        Command::RecentDeployments { limit } => recent_deployments::handle(state, limit).await,
        Command::GetBuildLogs { deployment_id } => {
            get_build_logs::handle(state, deployment_id).await
        }
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
