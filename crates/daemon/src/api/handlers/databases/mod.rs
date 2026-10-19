//! Handlers de databases: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod managed_database;
pub mod migration;
pub mod shared_access;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::SharedAccessList { server_service_id } => {
            shared_access::list(state, server_service_id).await
        }
        Command::SharedAccessGrant {
            server_service_id,
            project_id,
        } => shared_access::grant(state, server_service_id, project_id).await,
        Command::SharedAccessRevoke {
            server_service_id,
            project_id,
        } => shared_access::revoke(state, server_service_id, project_id).await,
        Command::ManagedDatabaseList { server_service_id } => {
            managed_database::list(state, server_service_id).await
        }
        Command::ManagedDatabaseCreate(req) => managed_database::create(state, req).await,
        Command::ManagedDatabaseDelete { id } => managed_database::delete(state, id).await,
        Command::ManagedDatabaseListAll => managed_database::list_all(state).await,
        Command::MigrationStart(req) => migration::start(state, req).await,
        Command::MigrationGet { id } => migration::get(state, id).await,
        Command::MigrationList { project_id } => migration::list(state, project_id).await,
        Command::MigrationRollback { id } => migration::rollback(state, id).await,
        Command::MigrationDiscard { id } => migration::discard(state, id).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
