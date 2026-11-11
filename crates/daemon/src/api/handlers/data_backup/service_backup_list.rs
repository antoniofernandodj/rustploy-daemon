//! `Command::ServiceBackupList`: os backups de dados de um serviço no S3 e a última operação.

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(state: AppState, service_id: String) -> Response {
    Response::ServiceBackups(
        crate::features::data_backup::service_backup::list(state.deploy_ctx(), &service_id).await,
    )
}
