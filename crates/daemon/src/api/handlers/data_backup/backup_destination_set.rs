//! `Command::BackupDestinationSet`: grava o destino S3 (chave secreta cifrada). Não está disponível ao
//! agente MCP: a chave secreta passaria pela conversa (ver `api::agent::tools::proibido_ao_agente`).

use crate::bootstrap::state::AppState;
use shared::{BackupDestination, Response};

pub async fn handle(state: AppState, destination: BackupDestination) -> Response {
    match crate::features::data_backup::destination::save(&state.db, &state.secrets, destination)
        .await
    {
        Ok(view) => Response::BackupDestination(view),
        Err(e) => Response::err("InvalidBackupDestination", e.to_string()),
    }
}
