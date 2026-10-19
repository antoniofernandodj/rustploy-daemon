//! `Command::JobDelete`: apaga um job (e o registro de aquecimento dele) e limpa
//! o `pre_deploy_job_id` dos serviços que o usavam como gate.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;
use tracing::warn;

pub async fn handle(state: AppState, id: String) -> RpResponse {
    match crate::store::job::delete(&state.db, &id).await {
        Ok(true) => {
            // Cascade: qualquer serviço que referenciava este job como
            // pré-deploy check fica com uma referência órfã — sem isso o
            // próximo deploy falha em PreDeployCheck com "job não
            // encontrado" (ver docs/plano-pre-deploy-gate.md).
            if let Err(e) = crate::store::services::clear_pre_deploy_job(&state.db, &id).await {
                warn!(job_id = %id, error = %e, "job_delete: falha ao limpar pre_deploy_job_id órfão em serviços");
            }
            crate::features::jobs::warm::forget(&state.db, &id).await;
            RpResponse::Ok
        }
        Ok(false) => RpResponse::err("NotFound", "job not found"),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}
