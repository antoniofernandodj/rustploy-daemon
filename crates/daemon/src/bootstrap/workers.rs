//! Tarefas de fundo do daemon: backup de env, métricas, logs, GC do registry, watchdog, agendadores e renovação de TLS.

use crate::bootstrap::state::AppState;
use crate::engine::DockerClient;
use crate::features::ingress::TlsManager;
use crate::features::registry::storage::RegistryStorage;
use crate::features::{backup, deploy, jobs, maintenance, observability, registry};
use crate::platform::config::RustployConfig;
use crate::platform::event_bus::EventBus;
use crate::store::Db;
use std::sync::Arc;
use tracing::{info, warn};

/// Laços que só precisam de banco, Docker e barramento (sobem antes do `AppState`).
pub fn spawn_standalone(
    config: &RustployConfig,
    db: &Arc<Db>,
    docker: &Arc<DockerClient>,
    bus: &Arc<EventBus>,
    backup_dir: &std::path::Path,
) {
    // Env var backup loop
    {
        let db2 = db.clone();
        let bdir = backup_dir.to_path_buf();
        let interval_secs = config.env_backup.interval_secs;
        tokio::spawn(async move {
            backup::backup_loop(db2, bdir, interval_secs).await;
        });
    }

    // Metrics background task
    {
        let docker_inner = docker.inner.clone();
        let db2 = db.clone();
        let bus2 = bus.clone();
        let interval = config.metrics.interval_secs;
        tokio::spawn(async move {
            observability::metrics::collect_loop(Arc::new(docker_inner), db2, bus2, interval).await;
        });
    }

    // Container log streaming task
    {
        let docker_inner = Arc::new(docker.inner.clone());
        let db2 = db.clone();
        let bus2 = bus.clone();
        tokio::spawn(async move {
            observability::container_logs::stream_loop(docker_inner, db2, bus2).await;
        });
    }
}

/// Laços que dependem do `AppState` já montado.
pub fn spawn_stateful(
    state: &AppState,
    registry_storage: Option<Arc<RegistryStorage>>,
    tls: &Arc<TlsManager>,
) {
    // Worker da fila global de deploys (um por vez).
    {
        let state2 = state.clone();
        tokio::spawn(async move {
            deploy::queue::run_worker(state2).await;
        });
    }

    // GC diário do registry
    {
        let db2 = state.db.clone();
        let gc_storage = registry_storage;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Some(storage) = &gc_storage {
                    match registry::gc::run(&db2, storage).await {
                        Ok(r) if r.blobs_removed > 0 => tracing::info!(
                            blobs = r.blobs_removed,
                            bytes = r.bytes_freed,
                            "registry: GC diário liberou espaço"
                        ),
                        Err(e) => tracing::warn!(error = %e, "registry: falha no GC diário"),
                        _ => {}
                    }
                }
            }
        });
    }

    // Watchdog: detecta containers parados/removidos, tenta restart e redeploy
    {
        let state2 = state.clone();
        tokio::spawn(async move {
            observability::watchdog::watchdog_loop(state2).await;
        });
    }

    // Reconciliation loop: sincroniza status DB ↔ Docker a cada 30 segundos
    {
        let state2 = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                deploy::recovery::reconcile(
                    &state2.db,
                    &state2.docker,
                    &state2.ingress,
                    &state2.tls,
                )
                .await;
            }
        });
    }

    // Scheduler de jobs one-shot: verifica a cada 30s se algum job agendado venceu
    {
        let state2 = state.clone();
        tokio::spawn(async move {
            jobs::scheduler::scheduler_loop(state2).await;
        });
    }

    // Scheduler da limpeza automática de Docker (ver docs/plano-limpeza-automatica-docker.md).
    {
        let state2 = state.clone();
        tokio::spawn(async move {
            maintenance::scheduler::scheduler_loop(state2).await;
        });
    }

    // Loop de renovação de certificados TLS (a cada 12 horas)
    {
        let tls_renew = tls.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(12 * 3600));
            loop {
                interval.tick().await;
                match tls_renew.renew_expiring().await {
                    Ok(renewed) if !renewed.is_empty() => {
                        info!(domains = ?renewed, "TLS: certificados renovados");
                    }
                    Err(e) => {
                        warn!(error = %e, "TLS: erro na renovação periódica");
                    }
                    _ => {}
                }
            }
        });
    }
}
