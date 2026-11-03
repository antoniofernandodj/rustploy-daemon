//! `Command::EnvKeyRename`: renomeia a chave de uma env (projeto ou serviço) mantendo o valor.
//! Atômico; o valor — `Plain` ou `Secret` — não passa pelo chamador.

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
        .rename_env_key(&project_id, service_id.as_deref(), &from, &to)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("EnvKeyRenameFailed", e.to_string()),
    }
}
