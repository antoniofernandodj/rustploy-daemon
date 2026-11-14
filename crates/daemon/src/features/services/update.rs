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
    let stored = match crate::store::services::get(&state.db, &id).await {
        Ok(Some(s)) => Some(s.spec),
        _ => None,
    };
    if let Some(stored) = &stored {
        clear_stale_mirror(stored, &mut spec);
    }
    // Porta externa: sentinela 0 → aloca; manual duplicada → erro.
    if let Err(e) =
        crate::features::ingress::external_ports::resolve_host_port(&state.db, &mut spec, Some(&id))
            .await
    {
        return RpResponse::err("PortAllocationError", e);
    }
    let old_ports = stored
        .as_ref()
        .map(crate::features::ingress::external_ports::host_ports)
        .unwrap_or_default();

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
            // Portas TCP ou domínios mudaram: as rotas do proxy só eram registradas no próximo deploy, e
            // uma porta recém-adicionada parecia quebrada até lá. Recalcula agora, em segundo plano (o
            // mesmo `IngressReconcile` da tela de Ingress).
            if stored
                .as_ref()
                .is_some_and(|old| routes_changed(old, &s.spec))
            {
                let (db, docker, ingress, tls) = (
                    state.db.clone(),
                    state.docker.clone(),
                    state.ingress.clone(),
                    state.tls.clone(),
                );
                tokio::spawn(async move {
                    crate::features::deploy::recovery::reconcile(&db, &docker, &ingress, &tls)
                        .await;
                });
            }
            RpResponse::Service(Box::new(s))
        }
        Ok(None) => RpResponse::err("NotFound", "service not found"),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}

/// As rotas que o proxy precisa (portas TCP com a porta do container, e domínios) mudaram?
pub(crate) fn routes_changed(old: &ServiceSpec, new: &ServiceSpec) -> bool {
    let tcp = |s: &ServiceSpec| -> Vec<(u16, u16)> {
        s.tcp_routes()
            .iter()
            .map(|r| (r.host_port, r.container_port(s.port)))
            .collect()
    };
    tcp(old) != tcp(new) || old.domain_routes() != new.domain_routes() || old.port != new.port
}

/// O cliente esvaziou a lista de portas TCP: o `host_port` que ainda vem no spec é só o **espelho
/// antigo** (a GUI não consegue mandar `null`, e o `/api/rpc` recusa um update sem um campo que o serviço
/// tem gravado). Sem isto, `materialize_tcp_ports` traria a porta de volta a partir dele.
///
/// Só vale quando o serviço gravado já tinha a lista e o `host_port` chega igual ao espelho gravado: um
/// cliente que muda o `host_port` (ou um spec antigo, sem lista) continua sendo levado a sério.
pub(crate) fn clear_stale_mirror(stored: &ServiceSpec, incoming: &mut ServiceSpec) {
    if !stored.tcp_ports.is_empty()
        && incoming.tcp_ports.is_empty()
        && incoming.host_port.is_some()
        && incoming.host_port == stored.host_port
    {
        incoming.host_port = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::TcpPortRoute;

    fn spec(host_port: Option<u16>, tcp: &[(u16, Option<u16>)]) -> ServiceSpec {
        let mut s: ServiceSpec = serde_json::from_value(serde_json::json!({
            "name": "s3", "project_id": "p", "source": {"Registry": {"image": "x"}},
            "port": 9000, "domain": null, "env_vars": [], "volumes": [],
            "healthcheck": shared::Healthcheck::default(), "replicas": 1,
            "resources": shared::ResourceLimits::default(),
        }))
        .unwrap();
        s.host_port = host_port;
        s.tcp_ports = tcp
            .iter()
            .map(|(h, c)| TcpPortRoute {
                host_port: *h,
                container_port: *c,
                label: None,
            })
            .collect();
        s
    }

    /// Adicionar a porta do painel muda as rotas; trocar só o rótulo, não.
    #[test]
    fn rotas_mudam_com_porta_nova_mas_nao_com_rotulo() {
        let antes = spec(Some(20003), &[(20003, None)]);
        let depois = spec(Some(20003), &[(20003, None), (20000, Some(15672))]);
        assert!(routes_changed(&antes, &depois));
        let mut so_rotulo = antes.clone();
        so_rotulo.tcp_ports[0].label = Some("AMQP".into());
        assert!(!routes_changed(&antes, &so_rotulo));
    }

    /// O caso que motivou: a GUI apaga a última porta e o espelho antigo vem junto.
    #[test]
    fn lista_esvaziada_descarta_o_espelho_antigo() {
        let gravado = spec(Some(20002), &[(20002, None)]);
        let mut chegando = spec(Some(20002), &[]);
        clear_stale_mirror(&gravado, &mut chegando);
        chegando.materialize_tcp_ports();
        assert!(chegando.tcp_routes().is_empty(), "a porta não pode voltar");
        assert_eq!(chegando.host_port, None);
    }

    #[test]
    fn host_port_mudado_ou_spec_antigo_continua_valendo() {
        // cliente antigo trocou a porta única: vale a nova
        let gravado = spec(Some(20002), &[(20002, None)]);
        let mut chegando = spec(Some(20009), &[]);
        clear_stale_mirror(&gravado, &mut chegando);
        assert_eq!(chegando.host_port, Some(20009));
        // serviço que ainda não tinha a lista (só o legado): nada muda
        let gravado = spec(Some(20002), &[]);
        let mut chegando = spec(Some(20002), &[]);
        clear_stale_mirror(&gravado, &mut chegando);
        assert_eq!(chegando.host_port, Some(20002));
        // lista ainda com portas: o espelho é recalculado depois, nada a fazer aqui
        let gravado = spec(Some(20002), &[(20002, None)]);
        let mut chegando = spec(Some(20002), &[(20002, None), (20003, Some(9001))]);
        clear_stale_mirror(&gravado, &mut chegando);
        assert_eq!(chegando.host_port, Some(20002));
    }
}
