//! O que o motor de deploy (fila, executor, start, stop) precisa do resto do daemon.

use crate::engine::DockerClient;
use crate::features::deploy::queue::DeployQueue;
use crate::features::ingress::{IngressController, TlsManager};
use crate::platform::{event_bus::EventBus, secrets::SecretsManager};
use crate::store::Db;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Handles de abort para deploys activos: deployment_id → AbortHandle.
/// Permite cancelar a task do executor ao receber DeployAbort.
pub type ActiveDeploys = Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>;

#[derive(Clone)]
pub struct DeployContext {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub ingress: Arc<IngressController>,
    pub bus: Arc<EventBus>,
    pub secrets: Arc<SecretsManager>,
    pub tls: Arc<TlsManager>,
    pub db_path: PathBuf,
    pub drain_secs: u64,
    pub registry_internal_token: Option<Arc<str>>,
    pub active_deploys: ActiveDeploys,
    pub deploy_queue: Arc<DeployQueue>,
}
