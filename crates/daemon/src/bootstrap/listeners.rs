//! Listeners do daemon: registry OCI embutido (storage e rota), proxy de ingress e API HTTP.

use crate::api;
use crate::bootstrap::state::AppState;
use crate::db;
use crate::features::ingress::{self, IngressController, TlsManager};
use crate::features::registry::{self, storage::RegistryStorage};
use crate::platform::config::RustployConfig;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{info, warn};

/// Storage do registry OCI embutido, criado ANTES do `AppState`: o listener HTTP do registry,
/// o handler `RegistryGc` e o job diário de GC precisam compartilhar a MESMA instância
/// (mesma trava de commit).
pub fn prepare_registry_storage(
    config: &RustployConfig,
    db_path: &Path,
) -> Option<Arc<RegistryStorage>> {
    if !config.registry.enabled {
        return None;
    }
    let storage_dir = config
        .registry
        .storage_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| db_path.join("registry"));
    match RegistryStorage::new(storage_dir) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::error!(error = %e, "registry: falha ao preparar storage_dir, registry desabilitado");
            None
        }
    }
}

/// Domínio público do registry — banco tem precedência sobre o config (mesmo padrão do e-mail
/// ACME). Só registra rota/cert se o storage subiu de verdade.
pub async fn publish_registry_route(
    config: &RustployConfig,
    db: &db::Db,
    ingress: &IngressController,
    tls: &Arc<TlsManager>,
) {
    let mut registry_domain = config.registry.domain.clone();
    if let Ok(Some(d)) =
        db::daemon_settings::get(db, db::daemon_settings::KEY_REGISTRY_DOMAIN).await
        && !d.trim().is_empty()
    {
        registry_domain = Some(d);
    }
    if let Some(domain) = registry_domain {
        let port = config.registry.port;
        ingress.upsert_route(&domain, vec![format!("127.0.0.1:{port}")], "rp-registry");
        let tls2 = tls.clone();
        let d = domain.clone();
        tokio::spawn(async move {
            if let Err(e) = tls2.ensure_cert(&d).await {
                warn!(domain = %d, error = %e, "registry: falha ao provisionar certificado");
            } else {
                info!(domain = %d, "registry: certificado provisionado");
            }
        });
    }
}

/// Sobe o proxy de ingress, a API HTTP/JSON + SSE e o listener do registry.
pub fn spawn(
    config: &'static RustployConfig,
    state: &AppState,
    ingress: &Arc<IngressController>,
    tls: &Arc<TlsManager>,
    registry_storage: Option<Arc<RegistryStorage>>,
) {
    // Ingress Proxy: roteamento de domínios e portas.
    // O listener HTTPS sobe sempre — certs chegam via ACME dinamicamente.
    {
        let routes = ingress.table_handle();
        let http_port = config.ingress.http_port;
        let https_port = config.ingress.https_port;
        let tls = tls.clone();
        tokio::spawn(async move {
            ingress::proxy::start_proxy(routes, http_port, https_port, Some(tls)).await;
        });
    }

    // API HTTP/JSON + SSE — canal administrativo remoto; é o que o GUI (glacier-ui/Luau) e
    // clientes externos consomem. Bind loopback + HTTP puro por padrão (atrás do ingress); se
    // `api.domain` estiver setado, o próprio listener termina TLS na sua porta com cert
    // automático via ACME. Serve TAMBÉM as rotas públicas sem Bearer (`api::public_routes`): o
    // webhook de CI/CD e o callback OAuth do Gitea (ver docs/plano-unificacao-webhook-api.md).
    // Com a API desabilitada (`[api] enabled = false`), não há webhook.
    if config.api.enabled {
        let state2 = state.clone();
        let api_cfg = config.api.clone();
        let tls2 = tls.clone();
        tokio::spawn(async move {
            api::http_api::run(state2, api_cfg, Some(tls2)).await;
        });
        // O token de conexão não vai para o log: ele contém o token, e o journal é lido por mais
        // gente do que o config.toml. Vai para o próprio config.toml, abaixo do `token` de `[api]`.
        if let Err(e) = api::agent::write_connection_token(config) {
            warn!(error = %e, "não deu para gravar o token de conexão no config.toml");
        }
        info!(
            "token de conexão (GUI, webui e agentes): [api].connection_token no config.toml, \
             ou `rustployd access-key`"
        );
    }

    // Registry OCI embutido (ver docs/plano-registry-embutido.md). Bind falho apenas loga e
    // desabilita o listener, como os demais listeners opcionais acima.
    if let Some(storage) = registry_storage {
        let db2 = state.db.clone();
        let port = config.registry.port;
        tokio::spawn(async move {
            registry::http::run(db2, storage, port).await;
        });
    }

    // Todo listener roda em `tokio::spawn`, então `run` mantém o processo vivo com um pending.
}
