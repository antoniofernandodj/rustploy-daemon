//! O agendador dos backups automáticos (`docs/plano-backup-de-dados-para-s3.md`, Fase 3): a cada minuto
//! olha os serviços com política ligada e dispara o que venceu. Mesmo formato do ticker dos jobs.
//!
//! Cada backup pega a vaga da fila de deploy (`acquire_job_slot`, em `service_backup`), então não roda
//! junto com um build nem com outro backup. Um disparo que falha (serviço ocupado, S3 fora) fica no
//! "último resultado" do serviço e a próxima tentativa é no horário seguinte da política: o agendador
//! não martela.

use super::{policy, service_backup};
use crate::features::deploy::DeployContext;
use chrono::Utc;
use std::time::Duration;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info};

const TICK_SECS: u64 = 60;

pub async fn scheduler_loop(ctx: DeployContext) {
    let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let now = Utc::now();
        for (service_id, b) in policy::enabled(&ctx.db) {
            let last = service_backup::last_start(&ctx, &service_id).await;
            if !policy::is_due(&b, last, now) {
                continue;
            }
            info!(service_id = %service_id, "backup automático: está na hora");
            if let Err(e) = service_backup::start_backup(
                ctx.clone(),
                service_id.clone(),
                b.policy.stop,
                Some(b.policy.keep),
            )
            .await
            {
                // grava a tentativa mesmo assim: sem isso, uma falha de pré-condição faria o agendador
                // tentar de novo a cada minuto
                service_backup::note_attempt(&ctx, &service_id, &e.to_string()).await;
                debug!(service_id = %service_id, error = %e, "backup automático não iniciou");
            }
        }
    }
}
