//! `Command::ServiceBackupPolicySet`: grava a política de backup automático do serviço.

use crate::bootstrap::state::AppState;
use shared::{BackupPolicy, Response};

pub async fn handle(state: AppState, service_id: String, policy: Option<BackupPolicy>) -> Response {
    match crate::features::data_backup::service_backup::policy_set(
        state.deploy_ctx(),
        &service_id,
        policy,
    )
    .await
    {
        Ok(list) => Response::ServiceBackups(list),
        Err(e) => Response::err("InvalidBackupPolicy", format!("{e:#}")),
    }
}
