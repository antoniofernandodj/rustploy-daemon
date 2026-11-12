//! `Command::EnvKeyRename`: renomeia a chave de uma env (projeto ou serviço) mantendo o valor.
//! Atômico; o valor — `Plain` ou `Secret` — não passa pelo chamador.

use crate::bootstrap::state::AppState;
use crate::platform::secrets::Scope;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    job_id: Option<String>,
    from: String,
    to: String,
) -> Response {
    let scope = match Scope::from_ids(service_id.as_deref(), job_id.as_deref()) {
        Ok(scope) => scope,
        Err(e) => return Response::err("InvalidScope", e.to_string()),
    };
    match state
        .secrets
        .rename_env_key(&project_id, scope, &from, &to)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("EnvKeyRenameFailed", e.to_string()),
    }
}
