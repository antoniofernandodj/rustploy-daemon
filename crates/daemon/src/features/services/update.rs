//! `Command::ServiceUpdate`: grava um ServiceSpec novo (porta externa, TLS dos
//! domínios, firewall) sem redeployar.

use crate::features::services::ServicesContext;
use shared::{Response as RpResponse, ServiceSpec};
use tracing::warn;

pub async fn handle(state: ServicesContext, id: String, mut spec: ServiceSpec) -> RpResponse {
    if let Some(reason) = crate::platform::env_validation::validate(&spec.env_vars) {
        return RpResponse::err("InvalidInput", reason);
    }
    if let Err(resp) =
        crate::features::services::check_depends_on(&state.db, &spec, Some(&id)).await
    {
        return resp;
    }
    // Porta externa: sentinela 0 → aloca; manual duplicada → erro.
    if let Err(e) =
        crate::features::ingress::external_ports::resolve_host_port(&state.db, &mut spec, Some(&id))
            .await
    {
        return RpResponse::err("PortAllocationError", e);
    }
    let old_ports = match crate::store::services::get(&state.db, &id).await {
        Ok(Some(s)) => crate::features::ingress::external_ports::host_ports(&s.spec),
        _ => Vec::new(),
    };

    for route in spec.domain_routes().into_iter().filter(|r| r.tls) {
        let tls = state.tls.clone();
        let domain = route.domain.clone();
        tokio::spawn(async move {
            if let Err(e) = tls.ensure_cert(&domain).await {
                warn!(domain = %domain, error = %e, "TLS: falha ao provisionar certificado ao atualizar serviço");
            }
        });
    }

    match crate::store::services::update_spec(&state.db, &id, spec).await {
        Ok(Some(s)) => {
            sync_firewall(
                &state,
                &s.id,
                &old_ports,
                &crate::features::ingress::external_ports::host_ports(&s.spec),
            )
            .await;
            RpResponse::Service(Box::new(s))
        }
        Ok(None) => RpResponse::err("NotFound", "service not found"),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}

/// Portas que saíram da lista → fecha cada uma (se mais ninguém a usa) e o listener do ingress; portas
/// que ficaram ou entraram → garante a liberação. Reusado por `manifest_apply` (IaC import), que também
/// atualiza specs fora deste handler.
pub(crate) async fn sync_firewall(state: &ServicesContext, id: &str, old: &[u16], new: &[u16]) {
    for old_port in old.iter().copied().filter(|o| !new.contains(o)) {
        state.ingress.remove_port_route(old_port);
        if !crate::features::ingress::external_ports::port_in_use_by_other(
            &state.db,
            old_port,
            Some(id),
        )
        .await
        {
            crate::features::ingress::firewall::ensure_denied_bg(old_port);
        }
    }
    for new_port in new.iter().copied() {
        crate::features::ingress::firewall::ensure_allowed_bg(new_port);
    }
}
