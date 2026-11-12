//! `Command::SecretList`: nomes (nunca valores) dos secrets de um projeto, ou de um serviço.

use crate::bootstrap::state::AppState;
use crate::platform::secrets::Scope;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    job_id: Option<String>,
) -> Response {
    let scope = match Scope::from_ids(service_id.as_deref(), job_id.as_deref()) {
        Ok(scope) => scope,
        Err(e) => return Response::err("InvalidScope", e.to_string()),
    };
    match state.secrets.list_names_scoped(&project_id, scope).await {
        Ok(names) => Response::SecretNames(names),
        Err(e) => Response::err("SecretListFailed", e.to_string()),
    }
}
