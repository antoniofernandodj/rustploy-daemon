//! `Command::ProjectServicesReorder`: ordem de exibição dos serviços de um projeto.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, project_id: String, service_ids: Vec<String>) -> RpResponse {
    match crate::store::services::reorder(&state.db, &project_id, &service_ids).await {
        Ok(Ok(())) => RpResponse::Ok,
        Ok(Err(reason)) => RpResponse::err("InvalidInput", reason),
        Err(e) => RpResponse::err(
            "DatabaseError",
            crate::platform::errors::humanize_db_error(&e, "serviço"),
        ),
    }
}
