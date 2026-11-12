//! `Command::SecretReveal`: o valor de um secret ("ver valor"). Grava auditoria; **não** está
//! disponível ao agente MCP (ver `api::agent::tools::proibido_ao_agente`).

use crate::bootstrap::state::AppState;
use crate::platform::secrets::Scope;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    job_id: Option<String>,
    name: String,
) -> Response {
    let scope = match Scope::from_ids(service_id.as_deref(), job_id.as_deref()) {
        Ok(scope) => scope,
        Err(e) => return Response::err("InvalidScope", e.to_string()),
    };
    match state.secrets.reveal(&project_id, scope, &name).await {
        Ok(value) => Response::SecretValue(value),
        Err(e) => Response::err("SecretRevealFailed", e.to_string()),
    }
}
