//! `Command::SecretMove`: mover entre env e secret, ou entre escopos (R5 do plano). Atômico.

use crate::bootstrap::state::AppState;
use crate::platform::secrets::Scope;
use shared::{Response, SecretMoveOp};

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    job_id: Option<String>,
    name: String,
    op: SecretMoveOp,
) -> Response {
    let scope = match Scope::from_ids(service_id.as_deref(), job_id.as_deref()) {
        Ok(scope) => scope,
        Err(e) => return Response::err("InvalidScope", e.to_string()),
    };
    match state
        .secrets
        .move_secret(&project_id, scope, &name, op)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("SecretMoveFailed", e.to_string()),
    }
}
