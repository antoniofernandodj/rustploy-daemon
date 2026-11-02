//! `Command::SecretList`: nomes (nunca valores) dos secrets de um projeto, ou de um serviço.

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(state: AppState, project_id: String, service_id: Option<String>) -> Response {
    match state
        .secrets
        .list_names_scoped(&project_id, service_id.as_deref())
        .await
    {
        Ok(names) => Response::SecretNames(names),
        Err(e) => Response::err("SecretListFailed", e.to_string()),
    }
}
