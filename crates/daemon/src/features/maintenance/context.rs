//! O que a limpeza do Docker precisa do resto do daemon.

use crate::engine::{DockerClient, inventory_cache::DockerCache};
use crate::features::deploy::queue::DeployQueue;
use crate::platform::{event_bus::EventBus, secrets::SecretsManager};
use crate::store::Db;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct MaintenanceContext {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub bus: Arc<EventBus>,
    pub docker_cache: Arc<DockerCache>,
    pub deploy_queue: Arc<DeployQueue>,
    pub secrets: Arc<SecretsManager>,
    pub db_path: PathBuf,
    /// Serviços com aquecimento em andamento (ou esperando a vaga da fila) — ver
    /// `service_warm`.
    pub warming_services: Arc<Mutex<HashSet<String>>>,
}
