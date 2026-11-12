//! `Command::SecretSet`: cria/sobrescreve um secret cifrado (do projeto, ou de um serviço).

use crate::bootstrap::state::AppState;
use crate::platform::secrets::Scope;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    job_id: Option<String>,
    name: String,
    value: String,
) -> Response {
    let scope = match Scope::from_ids(service_id.as_deref(), job_id.as_deref()) {
        Ok(scope) => scope,
        Err(e) => return Response::err("InvalidScope", e.to_string()),
    };
    match state
        .secrets
        .set_scoped(&project_id, scope, &name, &value)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("SecretSetFailed", e.to_string()),
    }
}
