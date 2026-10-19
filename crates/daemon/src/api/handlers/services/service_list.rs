//! `Command::ServiceList`: serviços de um projeto.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, project_id: String) -> RpResponse {
    match crate::store::services::list(&state.db, &project_id).await {
        Ok(ss) => RpResponse::Services(ss),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}
