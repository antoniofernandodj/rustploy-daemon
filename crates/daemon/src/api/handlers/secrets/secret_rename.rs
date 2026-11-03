//! `Command::SecretRename`: renomeia um secret e reaponta as envs que o referenciam. Atômico.
//! O valor cifrado não é lido, então o agente pode usar sem ver nada.

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    from: String,
    to: String,
) -> Response {
    match state
        .secrets
        .rename_secret(&project_id, service_id.as_deref(), &from, &to)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("SecretRenameFailed", e.to_string()),
    }
}
