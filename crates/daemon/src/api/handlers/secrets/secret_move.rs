//! `Command::SecretMove`: mover entre env e secret, ou entre escopos (R5 do plano). Atômico.

use crate::bootstrap::state::AppState;
use shared::{Response, SecretMoveOp};

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    name: String,
    op: SecretMoveOp,
) -> Response {
    match state
        .secrets
        .move_secret(&project_id, service_id.as_deref(), &name, op)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("SecretMoveFailed", e.to_string()),
    }
}
