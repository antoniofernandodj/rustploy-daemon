//! O que o manifesto e o pacote de serviço (export/import) precisam do resto do daemon.

use crate::features::deploy::DeployContext;
use crate::features::services::ServicesContext;
use crate::platform::config::{ApiConfig, outbound_ip};
use crate::platform::secrets::SecretsManager;
use crate::store::Db;
use std::sync::Arc;

#[derive(Clone)]
pub struct IacContext {
    pub db: Arc<Db>,
    pub secrets: Arc<SecretsManager>,
    pub api: ApiConfig,
    pub services: ServicesContext,
    pub deploy: DeployContext,
}

impl IacContext {
    /// URL pública do daemon, sem barra final (base do webhook de cada serviço).
    pub fn public_base_url(&self) -> String {
        self.api.public_base_url(&outbound_ip())
    }
}
