//! O que a limpeza do Docker precisa do resto do daemon.

use crate::engine::{DockerClient, inventory_cache::DockerCache};
use crate::features::deploy::queue::DeployQueue;
use crate::platform::event_bus::EventBus;
use crate::store::Db;
use std::sync::Arc;

#[derive(Clone)]
pub struct MaintenanceContext {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub bus: Arc<EventBus>,
    pub docker_cache: Arc<DockerCache>,
    pub deploy_queue: Arc<DeployQueue>,
}
