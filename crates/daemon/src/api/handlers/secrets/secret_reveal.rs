//! `Command::SecretReveal`: o valor de um secret ("ver valor"). Grava auditoria; **não** está
//! disponível ao agente MCP (ver `api::agent::tools::proibido_ao_agente`).

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    name: String,
) -> Response {
    match state
        .secrets
        .reveal(&project_id, service_id.as_deref(), &name)
        .await
    {
        Ok(value) => Response::SecretValue(value),
        Err(e) => Response::err("SecretRevealFailed", e.to_string()),
    }
}
