//! `Command::BackupDestinationGet`: o destino S3 dos backups, sem a chave secreta.

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(state: AppState) -> Response {
    Response::BackupDestination(
        crate::features::data_backup::destination::view(&state.db, &state.secrets).await,
    )
}
