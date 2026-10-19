//! `Command::ServiceGet`: um serviço pelo id.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, id: String) -> RpResponse {
    match crate::store::services::get(&state.db, &id).await {
        Ok(Some(s)) => RpResponse::Service(Box::new(s)),
        Ok(None) => RpResponse::err("NotFound", "service not found"),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}
