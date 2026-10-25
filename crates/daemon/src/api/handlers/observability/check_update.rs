//! `Command::CheckUpdate`: há release mais nova que este daemon? Só consulta, nunca instala.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, force: bool) -> RpResponse {
    match state.update_checker.check(force).await {
        Ok(info) => RpResponse::UpdateInfo(Box::new(info)),
        Err(error) => RpResponse::err("UpdateCheckFailed", error.to_string()),
    }
}
