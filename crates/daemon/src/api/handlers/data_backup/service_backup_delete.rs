//! `Command::ServiceBackupDelete`: apaga um backup do S3. Não está disponível ao agente MCP.

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(state: AppState, service_id: String, key: String) -> Response {
    match crate::features::data_backup::service_backup::delete(
        state.deploy_ctx(),
        &service_id,
        &key,
    )
    .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("BackupDeleteFailed", format!("{e:#}")),
    }
}
