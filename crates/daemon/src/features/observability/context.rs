//! O que o watchdog precisa do resto do daemon.

use crate::engine::DockerClient;
use crate::features::deploy::queue::DeployQueue;
use crate::platform::event_bus::EventBus;
use crate::store::Db;
use std::sync::Arc;

#[derive(Clone)]
pub struct WatchdogContext {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub bus: Arc<EventBus>,
    pub deploy_queue: Arc<DeployQueue>,
}
