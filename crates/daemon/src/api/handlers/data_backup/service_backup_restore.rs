//! `Command::ServiceBackupRestore`: restaura um backup do S3 **substituindo** os dados atuais (em segundo
//! plano). Não está disponível ao agente MCP: é decisão de uma pessoa.

use crate::bootstrap::state::AppState;
use shared::Response;

pub async fn handle(state: AppState, service_id: String, key: String, confirm: bool) -> Response {
    if !confirm {
        return Response::err(
            "ConfirmRequired",
            "restaurar substitui os dados atuais do serviço: envie `confirm: true`",
        );
    }
    match crate::features::data_backup::service_backup::start_restore(
        state.deploy_ctx(),
        service_id,
        key,
    )
    .await
    {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("RestoreNotStarted", format!("{e:#}")),
    }
}
