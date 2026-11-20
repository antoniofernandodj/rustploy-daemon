//! Coleta periódica de métricas de CPU/memória/disco do host e por container,
//! publicadas no event bus.

use crate::domain::{ContainerMetricsPoint, SystemMetricsPoint};
use crate::protocol::Event;
use crate::{platform::event_bus::EventBus, store::Db};
use bollard::Docker;
use chrono::Utc;
use std::{collections::HashMap, sync::Arc, time::Duration};
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};
use tokio::time::interval;
use tracing::warn;

pub async fn collect_loop(
    docker: Arc<Docker>,
    db: Arc<Db>,
    bus: Arc<EventBus>,
    interval_secs: u64,
) {
    let mut ticker = interval(Duration::from_secs(interval_secs));
    let mut prev_cpu: HashMap<String, (u64, u64)> = HashMap::new();
    // Só CPU e memória: `System::new_all()` carregaria (e guardaria para sempre)
    // a lista de TODOS os processos do host — nome, cmdline, ambiente — que este
    // laço nunca usa. Num servidor com centenas de processos são alguns MB parados.
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(MemoryRefreshKind::nothing().with_ram()),
    );
    // A lista de discos é montada uma vez; a cada tick só os números são relidos.
    let mut disks = Disks::new_with_refreshed_list();

    loop {
        ticker.tick().await;

        // Ninguém escutando (nenhum cliente SSE conectado) → não coleta: as
        // métricas só existem para as telas de Monitoring/Em execução, e cada
        // tick custa um `docker stats` por serviço. O primeiro tick depois de um
        // cliente conectar mede a CPU média desde a última coleta — aceitável.
        if bus.receiver_count() == 0 {
            continue;
        }

        // ── Métricas do SO ───────────────────────────────────────────────────
        bus.publish(Event::SystemMetrics(collect_system_metrics(
            &mut sys, &mut disks,
        )));

        // ── Métricas por container ───────────────────────────────────────────
        let services = match crate::store::services::get_running(&db).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "metrics: failed to get running services");
                continue;
            }
        };

        for svc in services {
            let Some(container_id) = &svc.live_container_id else {
                continue;
            };

            match collect_container_metrics(&docker, container_id, &svc.id, &mut prev_cpu).await {
                Ok(metrics) => bus.publish(Event::ContainerMetrics(metrics)),
                Err(e) => warn!(
                    service = svc.spec.name,
                    error = %e,
                    "metrics collection failed"
                ),
            }
        }
    }
}

fn collect_system_metrics(sys: &mut System, disks: &mut Disks) -> SystemMetricsPoint {
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let cpu_percent = sys.global_cpu_usage() as f64;
    let mem_used = sys.used_memory();
    let mem_total = sys.total_memory();

    // Disco: usa o mount com mais espaço total (normalmente /)
    let (disk_used, disk_total) = {
        disks.refresh(true);
        disks
            .iter()
            .max_by_key(|d| d.total_space())
            .map(|d| (d.total_space() - d.available_space(), d.total_space()))
            .unwrap_or((0u64, 0u64))
    };

    let load = System::load_average();

    SystemMetricsPoint {
        cpu_percent,
        mem_used_bytes: mem_used,
        mem_total_bytes: mem_total,
        disk_used_bytes: disk_used,
        disk_total_bytes: disk_total,
        load_avg_1: load.one,
        load_avg_5: load.five,
        load_avg_15: load.fifteen,
        timestamp: Utc::now(),
    }
}

async fn collect_container_metrics(
    docker: &Docker,
    container_id: &str,
    service_id: &str,
    _prev_cpu: &mut HashMap<String, (u64, u64)>,
) -> anyhow::Result<ContainerMetricsPoint> {
    use bollard::container::StatsOptions;
    use futures::StreamExt;

    let mut stream = docker.stats(
        container_id,
        Some(StatsOptions {
            stream: false,
            one_shot: true,
        }),
    );

    let stats = stream
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("no stats"))??;

    let cpu_delta = stats
        .cpu_stats
        .cpu_usage
        .total_usage
        .saturating_sub(stats.precpu_stats.cpu_usage.total_usage);
    let system_delta = stats
        .cpu_stats
        .system_cpu_usage
        .unwrap_or(0)
        .saturating_sub(stats.precpu_stats.system_cpu_usage.unwrap_or(0));
    let num_cpus = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;
    let cpu_percent = if system_delta > 0 {
        (cpu_delta as f64 / system_delta as f64) * num_cpus * 100.0
    } else {
        0.0
    };

    let mem_used = stats.memory_stats.usage.unwrap_or(0);
    let mem_limit = stats.memory_stats.limit.unwrap_or(0);

    let (net_rx, net_tx) = stats
        .networks
        .as_ref()
        .map(|nets| {
            nets.values().fold((0u64, 0u64), |(rx, tx), net| {
                (rx + net.rx_bytes, tx + net.tx_bytes)
            })
        })
        .unwrap_or((0, 0));

    Ok(ContainerMetricsPoint {
        service_id: service_id.to_string(),
        container_id: container_id.to_string(),
        cpu_percent,
        mem_used_bytes: mem_used,
        mem_limit_bytes: mem_limit,
        net_rx_bytes: net_rx,
        net_tx_bytes: net_tx,
        timestamp: Utc::now(),
    })
}
