//! `Command::JobWarmStatus`: se as imagens e o cache de build de um job já estão
//! no host — a fonte única da verdade que as telas exibem (`jobs::warm::status`).

use crate::api::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState, job_id: String) -> RpResponse {
    let job = match crate::db::job::get(&state.db, &job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => return RpResponse::err("NotFound", "job not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    RpResponse::JobWarmStatus(crate::jobs::warm::status(&state, &job).await)
}
