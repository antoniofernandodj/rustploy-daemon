//! `Command::SecretSet`: cria/sobrescreve um secret cifrado (do projeto, ou de um serviço).

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    name: String,
    value: String,
) -> Response {
    match state
        .secrets
        .set_scoped(&project_id, service_id.as_deref(), &name, &value)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("SecretSetFailed", e.to_string()),
    }
}
