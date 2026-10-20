//! `Command::DockerCleanupConfig*` e `DockerCleanupRunNow`: config e disparo
//! manual da limpeza automática do Docker (`maintenance/`).

use crate::bootstrap::state::AppState;
use crate::features::maintenance;
use chrono::Utc;
use shared::{DockerCleanupConfig, Response as RpResponse};

pub async fn get_config(state: AppState) -> RpResponse {
    let config = match maintenance::run::load_config(&state.maintenance_ctx()).await {
        Ok(c) => c,
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    let last_run = maintenance::run::load_last_run(&state.maintenance_ctx()).await;
    RpResponse::DockerCleanupConfig { config, last_run }
}

pub async fn set_config(state: AppState, mut config: DockerCleanupConfig) -> RpResponse {
    // Recalculado no servidor (não confia no que o cliente mandou) — é o que
    // faz o scheduler saber quando é a próxima janela, sem recalcular a
    // partir de "agora" a cada tick (ver `DockerCleanupConfig::recompute_next_run`).
    config.recompute_next_run(Utc::now());
    if let Err(e) = maintenance::run::save_config(&state.maintenance_ctx(), &config).await {
        return RpResponse::err("DatabaseError", e.to_string());
    }
    let last_run = maintenance::run::load_last_run(&state.maintenance_ctx()).await;
    RpResponse::DockerCleanupConfig { config, last_run }
}

/// Dispara a limpeza fora do horário agendado (botão "Executar agora"), com
/// os recursos marcados atualmente — independente do interruptor geral
/// `enabled` (é um botão de teste, não liga o agendamento). Roda em
/// background, igual `Command::JobRunNow`: a tela acompanha o resultado pelo
/// `Event::DockerCleanupCompleted`, não pela resposta deste RPC. Espera a
/// vez na fila de deploy, como a limpeza agendada.
pub async fn run_now(state: AppState) -> RpResponse {
    let config = match maintenance::run::load_config(&state.maintenance_ctx()).await {
        Ok(c) => c,
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    if !config.any_resource_enabled() {
        return RpResponse::err(
            "NoResourceSelected",
            "marque pelo menos um recurso antes de executar",
        );
    }
    tokio::spawn(async move {
        let permit = state.deploy_queue.acquire_job_slot().await;
        maintenance::run::run_holding_slot(&state.maintenance_ctx(), config, permit).await;
    });
    RpResponse::Ok
}

/// `Command::ServiceWarmStatusList`: o status de aquecimento de cada serviço com
/// build (Git ou zip), para as telas mostrarem o selo "aquecido/frio".
pub async fn service_warm_status_list(state: AppState) -> RpResponse {
    match maintenance::service_warm::status_all(&state.maintenance_ctx()).await {
        Ok(statuses) => RpResponse::ServiceWarmStatuses(statuses),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}

/// `Command::ServiceWarm`: aquece um serviço sem fazer deploy dele
/// (`maintenance::service_warm::request`).
pub async fn service_warm(state: AppState, service_id: String, force: bool) -> RpResponse {
    let svc = match crate::store::services::get(&state.db, &service_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return RpResponse::err("NotFound", "service not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    match maintenance::service_warm::request(&state.maintenance_ctx(), svc, force).await {
        Ok(already_warm) => RpResponse::ServiceWarmRequested { already_warm },
        Err(message) => RpResponse::err("CannotWarm", message),
    }
}
