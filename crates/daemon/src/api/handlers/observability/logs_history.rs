//! `Command::LogsHistory`: o log de runtime **persistido** do serviço (sobrevive à troca de
//! container no deploy e ao rollback). Plano: `docs/plano-log-de-runtime-persistido.md`.

use crate::bootstrap::state::AppState;
use chrono::{DateTime, Utc};
use shared::Response as RpResponse;

pub async fn handle(
    state: AppState,
    service_id: String,
    before: Option<DateTime<Utc>>,
    limit: usize,
    containers: Vec<String>,
) -> RpResponse {
    match crate::store::services::get(&state.db, &service_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return RpResponse::err("NotFound", "serviço não encontrado"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    }
    let Some(store) = crate::platform::log_store::global() else {
        return RpResponse::err(
            "LogsDisabled",
            "o log persistido está desligado ([logs] enabled = false no config.toml)",
        );
    };
    let read =
        tokio::task::spawn_blocking(move || store.read(&service_id, before, limit, &containers))
            .await;
    match read {
        Ok((entries, next_before)) => RpResponse::LogsHistory {
            entries,
            next_before,
        },
        Err(e) => RpResponse::err("InternalError", e.to_string()),
    }
}
