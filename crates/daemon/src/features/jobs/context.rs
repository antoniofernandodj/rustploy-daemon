//! O que os jobs (runner, agendador, aquecimento) precisam do resto do daemon.

use crate::engine::DockerClient;
use crate::features::deploy::queue::DeployQueue;
use crate::platform::{event_bus::EventBus, secrets::SecretsManager};
use crate::store::Db;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Sinais de cancelamento para `job_run`s em execução: job_run_id → sender
/// de um `watch<bool>` (valor `true` = cancelar). Diferente de
/// `ActiveDeploys` (que aborta a TASK): abortar só a task não mata o
/// processo `docker compose up` filho — `tokio::process::Child` não é
/// `kill_on_drop` por padrão, então uma task abortada deixaria o processo
/// (e os containers que ele sobe) órfãos. O sinal aqui é observado dentro de
/// `docker::compose::run_once_up`, que mata o processo de verdade
/// (`Child::kill`) antes de retornar — o `down()` de sempre cuida de
/// desmontar containers/rede depois. Ver `docs/plano-cancelamento-de-jobs.md`.
pub type ActiveJobs = Arc<Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>>;

#[derive(Clone)]
pub struct JobsContext {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub bus: Arc<EventBus>,
    pub secrets: Arc<SecretsManager>,
    pub db_path: PathBuf,
    pub registry_internal_token: Option<Arc<str>>,
    pub active_jobs: ActiveJobs,
    /// Jobs com aquecimento em andamento (ou esperando a vaga da fila) — ver `warm`.
    pub warming_jobs: Arc<Mutex<HashSet<String>>>,
    pub deploy_queue: Arc<DeployQueue>,
}
