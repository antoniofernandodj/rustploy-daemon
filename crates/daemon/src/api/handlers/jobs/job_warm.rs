//! `Command::JobWarm`: aquece um job (baixa as imagens que faltam e popula o
//! cache de build) sem executá-lo — `jobs::warm::request`. Com `force`, reconstrói
//! do zero mesmo com o job quente.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, job_id: String, force: bool) -> RpResponse {
    let job = match crate::store::job::get(&state.db, &job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => return RpResponse::err("NotFound", "job not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };

    match crate::features::jobs::warm::request(&state.jobs_ctx(), job, force).await {
        Ok(already_warm) => RpResponse::JobWarmRequested { already_warm },
        Err(message) => RpResponse::err("AlreadyWarming", message),
    }
}
