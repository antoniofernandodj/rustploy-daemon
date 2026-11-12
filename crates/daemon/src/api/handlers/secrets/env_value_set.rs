//! `Command::EnvValueSet`: troca só o valor de uma env (projeto ou serviço), na mesma posição.
//! Atômico; a chave, a ordem e os comentários do editor `.env` não mudam.

use crate::bootstrap::state::AppState;
use crate::platform::secrets::Scope;
use shared::{EnvVarValue, Response};

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    job_id: Option<String>,
    key: String,
    value: EnvVarValue,
) -> Response {
    let scope = match Scope::from_ids(service_id.as_deref(), job_id.as_deref()) {
        Ok(scope) => scope,
        Err(e) => return Response::err("InvalidScope", e.to_string()),
    };
    match state
        .secrets
        .set_env_value(&project_id, scope, &key, value)
        .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("EnvValueSetFailed", e.to_string()),
    }
}
