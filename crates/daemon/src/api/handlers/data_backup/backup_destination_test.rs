//! `Command::BackupDestinationTest`: grava, lê e apaga um objeto pequeno no S3. Não está disponível ao
//! agente MCP (o rascunho traz a chave secreta).

use crate::bootstrap::state::AppState;
use shared::{BackupDestination, Response};

pub async fn handle(state: AppState, destination: Option<BackupDestination>) -> Response {
    match crate::features::data_backup::destination::test(&state.db, &state.secrets, destination)
        .await
    {
        Ok(()) => Response::Ok,
        // a causa inteira (credencial, bucket, rede…) é o que a pessoa precisa ler
        Err(e) => Response::err("BackupDestinationTestFailed", format!("{e:#}")),
    }
}
