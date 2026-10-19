//! Limpeza de containers, volumes, imagens, build cache e redes do Docker: o miolo que o botão manual
//! (`api::handlers::docker_prune`) e a limpeza automática (`scheduler`/`run`) compartilham.

use crate::bootstrap::state::AppState;
use crate::db::{Db, daemon_settings};
use bollard::{
    container::PruneContainersOptions, image::PruneImagesOptions, network::PruneNetworksOptions,
    volume::PruneVolumesOptions,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Teto de um `docker builder prune`: um processo pendurado seguraria a vaga
/// única da fila de deploy e travaria todos os deploys.
const BUILD_CACHE_PRUNE_TIMEOUT: Duration = Duration::from_secs(600);

/// Resultado tipado de uma limpeza — o denominador comum entre o botão manual
/// (embrulhado em `Response::PruneResult` pelo handler) e a limpeza automática (a limpeza automática, que chama
/// `*_core` diretamente e embrulha em `shared::DockerCleanupResourceResult`).
pub struct PruneStat {
    pub count: u32,
    pub reclaimed_bytes: u64,
}

pub async fn prune_containers_core(state: &AppState) -> Result<PruneStat, String> {
    let result = state
        .docker
        .inner
        .prune_containers(None::<PruneContainersOptions<String>>)
        .await;
    // Removing containers changes image in-use counts and network attachments.
    state.docker_cache.df.invalidate().await;
    state.docker_cache.networks.invalidate().await;
    match result {
        Ok(r) => Ok(PruneStat {
            count: r.containers_deleted.map(|v| v.len() as u32).unwrap_or(0),
            reclaimed_bytes: r.space_reclaimed.unwrap_or(0) as u64,
        }),
        Err(e) => Err(e.to_string()),
    }
}

pub async fn prune_volumes_core(state: &AppState, all: bool) -> Result<PruneStat, String> {
    let mut filters = std::collections::HashMap::new();
    if all {
        filters.insert("all", vec!["true"]);
    }
    let result = state
        .docker
        .inner
        .prune_volumes(Some(PruneVolumesOptions { filters }))
        .await;
    state.docker_cache.df.invalidate().await;
    match result {
        Ok(r) => Ok(PruneStat {
            count: r.volumes_deleted.map(|v| v.len() as u32).unwrap_or(0),
            reclaimed_bytes: r.space_reclaimed.unwrap_or(0) as u64,
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// `older_than_hours > 0` só considera imagens CRIADAS há mais tempo que isso
/// (filtro `until` do Docker, que olha a data de criação, não a do último uso);
/// `0` não filtra por idade.
pub async fn prune_images_core(
    state: &AppState,
    all: bool,
    older_than_hours: u32,
) -> Result<PruneStat, String> {
    let result = state
        .docker
        .inner
        .prune_images(Some(PruneImagesOptions {
            filters: image_prune_filters(all, older_than_hours),
        }))
        .await;
    state.docker_cache.df.invalidate().await;
    match result {
        Ok(r) => Ok(PruneStat {
            count: r.images_deleted.map(|v| v.len() as u32).unwrap_or(0),
            reclaimed_bytes: r.space_reclaimed.unwrap_or(0) as u64,
        }),
        Err(e) => Err(e.to_string()),
    }
}

fn image_prune_filters(
    all: bool,
    older_than_hours: u32,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut filters = std::collections::HashMap::new();
    if all {
        filters.insert("dangling".to_string(), vec!["false".to_string()]);
    }
    if older_than_hours > 0 {
        filters.insert("until".to_string(), vec![format!("{older_than_hours}h")]);
    }
    filters
}

fn build_cache_prune_args(older_than_hours: u32) -> Vec<String> {
    build_cache_prune_args_for(crate::docker::engine(), older_than_hours)
}

/// O Podman não tem `builder prune`: o cache do Buildah sai por
/// `system prune --build`.
fn build_cache_prune_args_for(engine: crate::docker::Engine, older_than_hours: u32) -> Vec<String> {
    let mut args = match engine {
        crate::docker::Engine::Docker => {
            vec!["builder".to_string(), "prune".to_string(), "-f".to_string()]
        }
        crate::docker::Engine::Podman => vec![
            "system".to_string(),
            "prune".to_string(),
            "-f".to_string(),
            "--build".to_string(),
        ],
    };
    if older_than_hours > 0 {
        args.push("--filter".to_string());
        args.push(format!("until={older_than_hours}h"));
    }
    args
}

/// `older_than_hours > 0` mantém o cache usado nas últimas N horas (BuildKit
/// guarda o que foi usado há menos que isso); `0` remove todo o cache sem uso.
/// O instante e o corte ficam registrados para `JobWarmStatus` saber se o
/// cache de um job sobreviveu.
pub async fn prune_build_cache_core(
    state: &AppState,
    older_than_hours: u32,
) -> Result<PruneStat, String> {
    let output = tokio::time::timeout(
        BUILD_CACHE_PRUNE_TIMEOUT,
        crate::docker::cli()
            .args(build_cache_prune_args(older_than_hours))
            .kill_on_drop(true)
            .output(),
    )
    .await;

    match output {
        Ok(Ok(out)) if out.status.success() => {
            record_build_cache_prune(&state.db, older_than_hours).await;
            // Tenta extrair "Total reclaimed space: X.XXX MB" da saída
            let text = String::from_utf8_lossy(&out.stdout);
            let reclaimed_bytes = parse_reclaimed_space(&text);
            Ok(PruneStat {
                count: 0,
                reclaimed_bytes,
            })
        }
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "docker builder prune passou de {}s e foi interrompido",
            BUILD_CACHE_PRUNE_TIMEOUT.as_secs()
        )),
    }
}

/// Última limpeza do cache de build, gravada em `daemon_settings`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuildCachePrune {
    pub at: DateTime<Utc>,
    pub older_than_hours: u32,
}

async fn record_build_cache_prune(db: &Db, older_than_hours: u32) {
    let marker = BuildCachePrune {
        at: Utc::now(),
        older_than_hours,
    };
    let Ok(json) = serde_json::to_string(&marker) else {
        return;
    };
    if let Err(e) =
        daemon_settings::set(db, daemon_settings::KEY_BUILD_CACHE_LAST_PRUNE, &json).await
    {
        tracing::warn!(error = %e, "docker_prune: falha ao registrar a limpeza do cache de build");
    }
}

pub async fn last_build_cache_prune(db: &Db) -> Option<BuildCachePrune> {
    let raw = daemon_settings::get(db, daemon_settings::KEY_BUILD_CACHE_LAST_PRUNE)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

fn parse_reclaimed_space(output: &str) -> u64 {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Total reclaimed space:") {
            let rest = rest.trim();
            // exemplos: "1.234 GB", "512 MB", "0B"
            if let Some((num, unit)) = rest.split_once(char::is_whitespace) {
                let n: f64 = num.parse().unwrap_or(0.0);
                return match unit.to_uppercase().as_str() {
                    "GB" | "GIB" => (n * 1_000_000_000.0) as u64,
                    "MB" | "MIB" => (n * 1_000_000.0) as u64,
                    "KB" | "KIB" => (n * 1_000.0) as u64,
                    _ => n as u64,
                };
            }
        }
    }
    0
}

pub async fn prune_networks_core(state: &AppState) -> Result<PruneStat, String> {
    let result = state
        .docker
        .inner
        .prune_networks(None::<PruneNetworksOptions<String>>)
        .await;
    state.docker_cache.networks.invalidate().await;
    match result {
        Ok(r) => Ok(PruneStat {
            count: r.networks_deleted.map(|v| v.len() as u32).unwrap_or(0),
            reclaimed_bytes: 0,
        }),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cache_prune_podman_usa_system_prune() {
        use crate::docker::Engine;
        assert_eq!(
            build_cache_prune_args_for(Engine::Podman, 24),
            ["system", "prune", "-f", "--build", "--filter", "until=24h"]
        );
    }

    #[test]
    fn zero_hours_prunes_all_build_cache() {
        assert_eq!(build_cache_prune_args(0), ["builder", "prune", "-f"]);
    }

    #[test]
    fn retention_hours_become_an_until_filter() {
        assert_eq!(
            build_cache_prune_args(168),
            ["builder", "prune", "-f", "--filter", "until=168h"]
        );
    }

    #[test]
    fn image_filters_combine_all_and_age() {
        assert!(image_prune_filters(false, 0).is_empty());
        let filters = image_prune_filters(true, 24);
        assert_eq!(filters["dangling"], ["false"]);
        assert_eq!(filters["until"], ["24h"]);
    }
}
