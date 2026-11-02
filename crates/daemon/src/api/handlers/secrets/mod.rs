//! Handlers de secrets: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod infra_audit;
pub mod secret_delete;
pub mod secret_list;
pub mod secret_move;
pub mod secret_reveal;
pub mod secret_set;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::SecretSet {
            project_id,
            service_id,
            name,
            value,
        } => secret_set::handle(state, project_id, service_id, name, value).await,
        Command::SecretDelete {
            project_id,
            service_id,
            name,
        } => secret_delete::handle(state, project_id, service_id, name).await,
        Command::SecretList {
            project_id,
            service_id,
        } => secret_list::handle(state, project_id, service_id).await,
        Command::SecretReveal {
            project_id,
            service_id,
            name,
        } => secret_reveal::handle(state, project_id, service_id, name).await,
        Command::SecretMove {
            project_id,
            service_id,
            name,
            op,
        } => secret_move::handle(state, project_id, service_id, name, op).await,
        Command::InfraAudit => infra_audit::handle(state).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
