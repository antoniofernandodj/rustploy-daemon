//! Estado compartilhado do daemon (`AppState`) e os caches do inventário Docker.

use crate::{
    engine::{DockerClient, inventory_cache::DockerCache},
    features::databases::MigrationContext,
    features::deploy::{ActiveDeploys, DeployContext},
    features::iac::IacContext,
    features::ingress::{IngressController, TlsManager},
    features::jobs::{ActiveJobs, JobsContext},
    features::maintenance::MaintenanceContext,
    features::observability::WatchdogContext,
    features::services::ServicesContext,
    platform::event_bus::EventBus,
    platform::secrets::SecretsManager,
    store::Db,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Pending OAuth handshakes: CSRF `state` → `provider_id`, consumed by the
/// `/oauth/gitea/callback` route once the user authorizes.
pub type OAuthStates = Arc<Mutex<HashMap<String, String>>>;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub ingress: Arc<IngressController>,
    pub bus: Arc<EventBus>,
    pub secrets: Arc<SecretsManager>,
    pub tls: Arc<TlsManager>,
    pub db_path: PathBuf,
    pub backup_dir: PathBuf,
    pub drain_secs: u64,
    /// Config do listener HTTP da API. Guardada porque é dela que sai a URL
    /// pública do daemon (ver [`AppState::public_base_url`]) — desde a
    /// unificação, webhook e callback OAuth são servidos por esse mesmo
    /// listener, então a base deles é a base da API.
    pub api: crate::platform::config::ApiConfig,
    pub started_at: std::time::Instant,
    pub oauth_states: OAuthStates,
    pub active_deploys: ActiveDeploys,
    /// Ver [`ActiveJobs`].
    pub active_jobs: ActiveJobs,
    /// Jobs com aquecimento em andamento (ou esperando a vaga da fila) — ver
    /// `jobs::warm`.
    pub warming_jobs: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Serviços com aquecimento em andamento — ver `maintenance::service_warm`.
    pub warming_services: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Fila global de deploys (um por vez). `deploy_start` enfileira aqui e o
    /// worker (`crate::features::deploy::queue::run_worker`) puxa serialmente.
    pub deploy_queue: Arc<crate::features::deploy::queue::DeployQueue>,
    /// TTL cache for the slow host-wide Docker inventory (see [`DockerCache`]).
    pub docker_cache: Arc<DockerCache>,
    /// Storage do registry OCI embutido — o MESMO (mesma `commit_lock`) que o
    /// listener HTTP do registry usa, para o handler `RegistryGc`. `None`
    /// quando `[registry]` está desabilitado na config.
    pub registry_storage: Option<Arc<crate::features::registry::storage::RegistryStorage>>,
    /// Token interno `rp-internal`, regenerado a cada boot (ver
    /// `crate::features::registry::internal_token`), usado pelo `DeployExecutor` pra se
    /// autenticar sozinho ao puxar imagens do registry embutido.
    pub registry_internal_token: Option<Arc<str>>,
    /// Verificação de versão nova, com cache (ver `features::update`).
    pub update_checker: Arc<crate::features::update::UpdateChecker>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<Db>,
        docker: Arc<DockerClient>,
        ingress: Arc<IngressController>,
        bus: Arc<EventBus>,
        secrets: Arc<SecretsManager>,
        tls: Arc<TlsManager>,
        db_path: PathBuf,
        backup_dir: PathBuf,
        drain_secs: u64,
        api: crate::platform::config::ApiConfig,
        update: &crate::platform::config::UpdateConfig,
        registry_storage: Option<Arc<crate::features::registry::storage::RegistryStorage>>,
        registry_internal_token: Option<Arc<str>>,
    ) -> Self {
        Self {
            db,
            docker,
            ingress,
            bus,
            secrets,
            tls,
            db_path,
            backup_dir,
            drain_secs,
            api,
            started_at: std::time::Instant::now(),
            oauth_states: Arc::new(Mutex::new(HashMap::new())),
            active_deploys: Arc::new(Mutex::new(HashMap::new())),
            active_jobs: Arc::new(Mutex::new(HashMap::new())),
            warming_jobs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            warming_services: Arc::new(Mutex::new(std::collections::HashSet::new())),
            deploy_queue: crate::features::deploy::queue::DeployQueue::new(),
            docker_cache: Arc::new(DockerCache::new()),
            registry_storage,
            registry_internal_token,
            update_checker: Arc::new(crate::features::update::UpdateChecker::new(update)),
        }
    }

    pub fn deploy_ctx(&self) -> DeployContext {
        DeployContext {
            db: self.db.clone(),
            docker: self.docker.clone(),
            ingress: self.ingress.clone(),
            bus: self.bus.clone(),
            secrets: self.secrets.clone(),
            tls: self.tls.clone(),
            db_path: self.db_path.clone(),
            drain_secs: self.drain_secs,
            registry_internal_token: self.registry_internal_token.clone(),
            active_deploys: self.active_deploys.clone(),
            deploy_queue: self.deploy_queue.clone(),
        }
    }

    pub fn jobs_ctx(&self) -> JobsContext {
        JobsContext {
            db: self.db.clone(),
            docker: self.docker.clone(),
            bus: self.bus.clone(),
            secrets: self.secrets.clone(),
            db_path: self.db_path.clone(),
            registry_internal_token: self.registry_internal_token.clone(),
            active_jobs: self.active_jobs.clone(),
            warming_jobs: self.warming_jobs.clone(),
            deploy_queue: self.deploy_queue.clone(),
        }
    }

    pub fn maintenance_ctx(&self) -> MaintenanceContext {
        MaintenanceContext {
            db: self.db.clone(),
            docker: self.docker.clone(),
            bus: self.bus.clone(),
            docker_cache: self.docker_cache.clone(),
            deploy_queue: self.deploy_queue.clone(),
            secrets: self.secrets.clone(),
            db_path: self.db_path.clone(),
            warming_services: self.warming_services.clone(),
        }
    }

    pub fn watchdog_ctx(&self) -> WatchdogContext {
        WatchdogContext {
            db: self.db.clone(),
            docker: self.docker.clone(),
            bus: self.bus.clone(),
            deploy_queue: self.deploy_queue.clone(),
        }
    }

    pub fn services_ctx(&self) -> ServicesContext {
        ServicesContext {
            db: self.db.clone(),
            docker: self.docker.clone(),
            ingress: self.ingress.clone(),
            tls: self.tls.clone(),
            deploy: self.deploy_ctx(),
        }
    }

    pub fn migration_ctx(&self) -> MigrationContext {
        MigrationContext {
            db: self.db.clone(),
            docker: self.docker.clone(),
            secrets: self.secrets.clone(),
            deploy: self.deploy_ctx(),
        }
    }

    pub fn iac_ctx(&self) -> IacContext {
        IacContext {
            db: self.db.clone(),
            secrets: self.secrets.clone(),
            api: self.api.clone(),
            services: self.services_ctx(),
            deploy: self.deploy_ctx(),
        }
    }

    /// URL pública do daemon, sem barra final: base do webhook
    /// (`{base}/webhook/{service_id}/{token}`) e do callback OAuth
    /// (`{base}/oauth/gitea/callback`). Derivada de `[api]` — não há setting no
    /// banco para sobrescrevê-la; para mudá-la, configure `api.domain`.
    pub fn public_base_url(&self) -> String {
        self.api
            .public_base_url(&crate::platform::config::outbound_ip())
    }
}
