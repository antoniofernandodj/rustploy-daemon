//! O que a migração de banco precisa do resto do daemon.

use crate::engine::DockerClient;
use crate::features::deploy::DeployContext;
use crate::platform::secrets::SecretsManager;
use crate::store::Db;
use std::sync::Arc;

#[derive(Clone)]
pub struct MigrationContext {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub secrets: Arc<SecretsManager>,
    pub deploy: DeployContext,
}
