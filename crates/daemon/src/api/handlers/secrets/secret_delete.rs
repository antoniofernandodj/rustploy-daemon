//! `Command::SecretDelete`: apaga um secret (do projeto, ou de um serviço).

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    name: String,
) -> Response {
    match state
        .secrets
        .delete_scoped(&project_id, service_id.as_deref(), &name)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("SecretDeleteFailed", e.to_string()),
    }
}
