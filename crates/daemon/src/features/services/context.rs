//! O que criar, atualizar e remover um serviço precisam do resto do daemon.

use crate::engine::DockerClient;
use crate::features::deploy::DeployContext;
use crate::features::ingress::{IngressController, TlsManager};
use crate::store::Db;
use std::sync::Arc;

#[derive(Clone)]
pub struct ServicesContext {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub ingress: Arc<IngressController>,
    pub tls: Arc<TlsManager>,
    pub deploy: DeployContext,
}
