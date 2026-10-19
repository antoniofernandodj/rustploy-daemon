//! `Command::ProjectCreate`: cria um projeto.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, name: String, description: Option<String>) -> RpResponse {
    match crate::db::projects::create(&state.db, name, description).await {
        Ok(p) => RpResponse::Project(p),
        Err(e) => RpResponse::err(
            "DatabaseError",
            crate::platform::errors::humanize_db_error(&e, "projeto"),
        ),
    }
}
