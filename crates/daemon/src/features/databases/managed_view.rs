//! Visão de um banco gerenciado: monta a connection URL como o projeto consumidor a enxerga.

use crate::features::databases::shared_db::Engine;
use crate::features::deploy::shared_net;
use crate::platform::connection::{self, ConnTarget};
use crate::store::managed_database::Row;
use shared::{ManagedDatabase, Service};

/// Como o projeto consumidor enxerga o servidor: o alias global (outro projeto)
/// ou a chave do YAML (o próprio projeto do servidor).
fn host_for(svc: &Service, project_id: &str) -> String {
    if project_id == svc.spec.project_id
        && let shared::ServiceSource::Compose(c) = &svc.spec.source
        && let Some(h) = connection::compose_host(
            &c.content,
            c.ingress_service.as_deref(),
            svc.spec.db_kind.as_deref(),
        )
    {
        return h;
    }
    shared_net::alias_of(svc)
}

pub fn to_view(svc: &Service, engine: Engine, r: &Row) -> ManagedDatabase {
    let host = host_for(svc, &r.project_id);
    let url = connection::connection_url(
        Some(engine.kind_id()),
        &ConnTarget {
            host: &host,
            port: svc.spec.port,
            database: Some(&r.name),
            user: Some(&r.name),
            password: Some(&r.password),
            auth_source: Some(&r.name),
        },
    );
    ManagedDatabase {
        id: r.id.clone(),
        server_service_id: r.server_service_id.clone(),
        project_id: r.project_id.clone(),
        name: r.name.clone(),
        env_var: r.env_var.clone(),
        connection_limit: r.connection_limit,
        statement_timeout_ms: r.statement_timeout_ms,
        connection_url: url,
        created_at: r.created_at,
    }
}
