//! `Command::EnvValueSet`: troca só o valor de uma env (projeto ou serviço), na mesma posição.
//! Atômico; a chave, a ordem e os comentários do editor `.env` não mudam.

use crate::bootstrap::state::AppState;
use shared::{EnvVarValue, Response};

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    key: String,
    value: EnvVarValue,
) -> Response {
    match state
        .secrets
        .set_env_value(&project_id, service_id.as_deref(), &key, value)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("EnvValueSetFailed", e.to_string()),
    }
}
