//! `Command::JobRunHistory`: últimas execuções de um job.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, job_id: String, limit: usize) -> RpResponse {
    match crate::store::job_run::list_for_job(&state.db, &job_id, limit).await {
        Ok(runs) => RpResponse::JobRuns(runs),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}
