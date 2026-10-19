//! Cache do inventário do host (imagens, volumes, redes), que é lento de consultar.

use crate::platform::ttl_cache::TtlCache;
use shared::{DockerImageInfo, DockerNetworkInfo, DockerVolumeInfo};
use std::time::Duration;

/// How long the host-wide Docker inventory (`docker system df` + the network
/// cross-reference) stays cached before the next request re-hits the Docker
/// Engine. These calls are slow (hundreds of ms to seconds) and the 2s status
/// poll would otherwise fire them every tick; the inventory changes rarely, so
/// a generous TTL cuts almost all of that Docker load. Busted early by the
/// prune handlers so a cleanup is reflected at once.
const DOCKER_CACHE_TTL: Duration = Duration::from_secs(300);

/// Caches the slow host-wide Docker inventory calls so the 2s status poll (and
/// every Docker-tab refresh) serves from RAM instead of re-hitting the Docker
/// Engine. See [`DOCKER_CACHE_TTL`].
pub struct DockerCache {
    /// `docker system df` feeds BOTH the Images and Volumes tabs — cached
    /// together so a single refresh serves both handlers.
    pub df: TtlCache<(Vec<DockerImageInfo>, Vec<DockerVolumeInfo>)>,
    /// The network list cross-referenced against every container's attachments.
    pub networks: TtlCache<Vec<DockerNetworkInfo>>,
}

impl DockerCache {
    pub fn new() -> Self {
        Self {
            df: TtlCache::new(DOCKER_CACHE_TTL),
            networks: TtlCache::new(DOCKER_CACHE_TTL),
        }
    }
}
