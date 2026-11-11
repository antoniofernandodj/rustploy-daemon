//! `Command::ServiceBackupNow`: começa o backup dos dados do serviço (em segundo plano). O resultado vem
//! em `ServiceBackupList(.last_run)`.

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(
    state: AppState,
    service_id: String,
    stop: bool,
    keep: Option<u32>,
) -> Response {
    match crate::features::data_backup::service_backup::start_backup(
        state.deploy_ctx(),
        service_id,
        stop,
        keep,
    )
    .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("BackupNotStarted", format!("{e:#}")),
    }
}
