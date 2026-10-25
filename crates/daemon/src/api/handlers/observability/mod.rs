//! Handlers de observability: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod apply_update;
pub mod check_update;
pub mod daemon_status;
pub mod ingress;
pub mod logs_get;
pub mod ping;
pub mod update_status;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::Ping => ping::handle(state).await,
        Command::DaemonStatus => daemon_status::handle(state).await,
        Command::CheckUpdate { force } => check_update::handle(state, force).await,
        Command::ApplyUpdate {
            allow_active_deploys,
        } => apply_update::handle(state, allow_active_deploys).await,
        Command::UpdateStatus => update_status::handle(state).await,
        Command::UpdateRollback => update_status::rollback(state).await,
        Command::IngressRoutes => ingress::routes(state).await,
        Command::IngressReconcile { service_id } => ingress::reconcile(state, service_id).await,
        Command::LogsGet { service_id, tail } => logs_get::handle(state, service_id, tail).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
