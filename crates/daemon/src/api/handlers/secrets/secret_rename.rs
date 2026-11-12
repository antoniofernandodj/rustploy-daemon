//! `Command::SecretRename`: renomeia um secret e reaponta as envs que o referenciam. Atômico.
//! O valor cifrado não é lido, então o agente pode usar sem ver nada.

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
        .rename_secret(&project_id, scope, &from, &to)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("SecretRenameFailed", e.to_string()),
    }
}
