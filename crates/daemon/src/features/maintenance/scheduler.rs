//! Ticker que verifica se a limpeza automática de Docker está devida —
//! mesmo idioma de `jobs::scheduler::scheduler_loop`, mas sem tabela própria:
//! a config é um singleton em `daemon_settings`, então cada tick só carrega
//! um JSON e compara `next_run_at` (persistido, nunca recalculado a partir
//! de "agora" — ver `DockerCleanupConfig::recompute_next_run`) contra `now`.

use crate::features::maintenance::MaintenanceContext;
use chrono::Utc;
use std::time::Duration;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

const TICK_SECS: u64 = 60;

pub async fn scheduler_loop(state: MaintenanceContext) {
    let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        let config = match super::run::load_config(&state).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "maintenance scheduler: falha ao carregar config");
                continue;
            }
        };

        let due = match config.next_run_at {
            Some(next) => Utc::now() >= next,
            None => false,
        };
        if !due {
            continue;
        }
        // Só roda com a vaga única da fila livre: sem isso a limpeza poderia
        // apagar o cache de um build em andamento. Ocupada ou pausada, o
        // `next_run_at` continua vencido e o próximo tick tenta de novo.
        let Some(permit) = state.deploy_queue.try_job_slot_now() else {
            info!(
                "maintenance: limpeza adiada, há deploy ou job em execução (ou a fila está pausada)"
            );
            continue;
        };
        super::run::run_holding_slot(&state, config, permit).await;
    }
}
