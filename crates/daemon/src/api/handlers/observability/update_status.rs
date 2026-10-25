//! `Command::UpdateStatus`: o passo da atualização em curso, ou o resultado da última.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState) -> RpResponse {
    RpResponse::UpdateProgress(Box::new(
        state.update_applier.status(env!("CARGO_PKG_VERSION")),
    ))
}

/// `Command::UpdateRollback`: o helper restaura os binários do último backup.
pub async fn rollback(state: AppState) -> RpResponse {
    match state.update_applier.rollback().await {
        Ok(()) => RpResponse::Ok,
        Err(message) => RpResponse::err("UpdateRollbackFailed", message),
    }
}
