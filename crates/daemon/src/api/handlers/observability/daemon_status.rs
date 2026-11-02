//! `Command::DaemonStatus`: versão, uptime e contagem de serviços
//! rodando/total.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

pub async fn handle(state: AppState) -> RpResponse {
    let services = crate::store::services::get_running(&state.db)
        .await
        .unwrap_or_default();

    let total: usize = state
        .db
        .infra()
        .snapshot()
        .projects
        .iter()
        .map(|p| p.services.len())
        .sum();

    RpResponse::DaemonStatus(shared::DaemonStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        services_running: services.len(),
        services_total: total,
    })
}
