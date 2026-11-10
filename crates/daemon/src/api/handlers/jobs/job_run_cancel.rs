//! `Command::JobRunCancel`: sinaliza o cancelamento de uma execução de job em
//! andamento.

use crate::bootstrap::state::AppState;
use shared::Response as RpResponse;

/// Sinaliza cancelamento pro `job_run` em `state.active_jobs` (ver
/// `docs/plano-cancelamento-de-jobs.md`) — quem observa o sinal e mata o
/// processo `docker compose up` de verdade é `docker::compose::run_once_up`.
/// Não remove a entrada do mapa: quem faz isso é a própria task ao terminar
/// (normalmente ou por cancelamento), como já acontece em `active_deploys`.
pub async fn handle(state: AppState, job_run_id: String) -> RpResponse {
    // Ainda esperando a vez na fila: sai da fila e fecha como cancelada (nunca começou).
    if let Some(cancelar) = state.deploy_queue.cancel_queued(&job_run_id) {
        cancelar.await;
        state.bus.publish(shared::Event::DeployQueueChanged);
        return RpResponse::Ok;
    }

    let sent = match state.active_jobs.lock() {
        Ok(map) => match map.get(&job_run_id) {
            Some(tx) => tx.send(true).is_ok(),
            None => false,
        },
        Err(_) => false,
    };

    if sent {
        RpResponse::Ok
    } else {
        RpResponse::err(
            "NotFound",
            "job_run não está em execução (já terminou ou o id é inválido)",
        )
    }
}

/// `DeployAbort` com o id de um item que não é deploy (o "✕"/"■" da fila e de "Executando" manda o
/// mesmo comando para qualquer item): `jrun_…` cancela a execução do job (na fila ou rodando) e
/// `jwarm_…` tira o aquecimento da fila. `None` se o id é de um deploy.
pub async fn abort_item(state: &AppState, id: &str) -> Option<RpResponse> {
    if id.starts_with("jrun_") {
        return Some(handle(state.clone(), id.to_string()).await);
    }
    if id.starts_with("jwarm_") {
        return Some(match state.deploy_queue.cancel_queued(id) {
            Some(cancelar) => {
                cancelar.await;
                state.bus.publish(shared::Event::DeployQueueChanged);
                RpResponse::Ok
            }
            None => RpResponse::err(
                "InvalidState",
                "o aquecimento já está em execução e não pode ser interrompido",
            ),
        });
    }
    None
}
