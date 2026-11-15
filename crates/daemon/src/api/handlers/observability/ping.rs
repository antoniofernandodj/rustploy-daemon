//! `Command::Ping`: responde `Pong` com o uptime.

use crate::bootstrap::state::AppState;
use crate::protocol::Response as RpResponse;

pub async fn handle(state: AppState) -> RpResponse {
    RpResponse::Pong {
        uptime_secs: state.started_at.elapsed().as_secs(),
    }
}
