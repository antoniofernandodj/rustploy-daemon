//! `Command::BackupDestinationClear`: apaga o destino gravado (os backups já enviados ficam no S3).

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(state: AppState) -> Response {
    match crate::features::data_backup::destination::clear(&state.db).await {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("DatabaseError", e.to_string()),
    }
}
