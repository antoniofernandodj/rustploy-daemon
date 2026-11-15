//! `Command::ServiceConnectionInfo`: host e connection string internos de um
//! serviço (ver `crate::platform::connection` e `docs/plano-banco-compartilhado.md` §3).

use crate::domain::ServiceSource;
use crate::platform::connection::{self, ConnTarget};
use crate::protocol::Response as RpResponse;
use crate::{bootstrap::state::AppState, engine::containers, features::deploy::env_resolve};
use std::collections::HashMap;

pub async fn handle(state: AppState, service_id: String) -> RpResponse {
    let svc = match crate::store::services::get(&state.db, &service_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return RpResponse::err("NotFound", "service not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    let env: HashMap<String, String> =
        match env_resolve::resolve(&state.db, &state.secrets, &svc).await {
            Ok(v) => v.into_iter().collect(),
            Err(e) => return RpResponse::err("EnvError", e.to_string()),
        };
    let kind = svc.spec.product.as_deref();

    let host = match &svc.spec.source {
        ServiceSource::Compose(c) => {
            match connection::compose_host(&c.content, c.ingress_service.as_deref(), kind) {
                Some(h) => h,
                // Sem certeza de qual serviço da stack é o banco: o nome do
                // container sempre resolve; inventar `rp_<nome>` não.
                None => {
                    let idx = containers::index_containers(&state.docker.inner).await;
                    idx.for_service(&svc)
                        .first()
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| crate::domain::app_network_alias(&svc.spec.name))
                }
            }
        }
        _ => crate::domain::app_network_alias(&svc.spec.name),
    };

    let (database, user, password, auth_source) =
        connection::credentials(kind, |k| env.get(k).cloned());
    let url = connection::connection_url(
        kind,
        &ConnTarget {
            host: &host,
            port: svc.spec.port,
            database: database.as_deref(),
            user: user.as_deref(),
            password: password.as_deref(),
            auth_source,
        },
    );
    RpResponse::ConnectionInfo {
        host,
        internal_url: url,
    }
}
