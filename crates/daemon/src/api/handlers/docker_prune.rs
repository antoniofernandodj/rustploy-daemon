//! `Command::Prune*`: limpeza manual de containers, volumes, imagens, build cache e redes; a lógica
//! mora em `features::maintenance::prune`.

use crate::bootstrap::state::AppState;
use crate::features::maintenance::prune::{self, PruneStat};
use shared::Response as RpResponse;

fn respond(result: Result<PruneStat, String>) -> RpResponse {
    match result {
        Ok(s) => RpResponse::PruneResult {
            count: s.count,
            reclaimed_bytes: s.reclaimed_bytes,
        },
        Err(e) => RpResponse::err("DockerError", e),
    }
}

pub async fn prune_containers(state: AppState) -> RpResponse {
    respond(prune::prune_containers_core(&state).await)
}

pub async fn prune_volumes(state: AppState, all: bool) -> RpResponse {
    respond(prune::prune_volumes_core(&state, all).await)
}

pub async fn prune_images(state: AppState, all: bool) -> RpResponse {
    respond(prune::prune_images_core(&state, all, 0).await)
}

pub async fn prune_build_cache(state: AppState) -> RpResponse {
    respond(prune::prune_build_cache_core(&state, 0).await)
}

pub async fn prune_networks(state: AppState) -> RpResponse {
    respond(prune::prune_networks_core(&state).await)
}
