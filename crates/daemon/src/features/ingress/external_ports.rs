//! Alocação automática de portas externas (`ServiceSpec.tcp_ports`, e o `host_port` legado).
//!
//! `host_port = 0` numa rota é a sentinela "aloque para mim": aqui ela é trocada
//! por uma porta livre da faixa `[external_ports]` da config, persistida no
//! spec (redeploys/restarts reusam a mesma porta para sempre). Portas manuais
//! passam por validação de duplicata contra os demais serviços e entre si.

use crate::platform::config::RustployConfig;
use crate::store::Db;
use shared::{MAX_TCP_PORTS, ServiceSpec, TcpPortRoute};

/// Sentinela em `TcpPortRoute.host_port` (e no `host_port` legado) que pede alocação automática.
pub const AUTO_PORT: u16 = 0;

/// Resolve as portas TCP do spec antes de persistir:
/// - move o `host_port` legado para `tcp_ports` (e espelha de volta; ver `materialize_tcp_ports`);
/// - porta `0` → aloca uma livre da faixa configurada, sem repetir as outras do próprio spec;
/// - porta manual → erro se outro serviço (≠ `exclude_id`) ou outra rota do mesmo spec já a usa;
/// - mais de [`MAX_TCP_PORTS`] → erro.
///
/// Um cliente que edita a lista nova manda `host_port: null`: o espelho é recalculado aqui. Sem isso,
/// esvaziar a lista traria de volta a porta pelo espelho antigo.
pub async fn resolve_host_port(
    db: &Db,
    spec: &mut ServiceSpec,
    exclude_id: Option<&str>,
) -> Result<(), String> {
    spec.materialize_tcp_ports();
    if spec.tcp_ports.is_empty() {
        return Ok(());
    }
    if spec.tcp_ports.len() > MAX_TCP_PORTS {
        return Err(format!(
            "no máximo {MAX_TCP_PORTS} portas TCP por serviço ({} pedidas)",
            spec.tcp_ports.len()
        ));
    }
    let mut used = used_ports(db, exclude_id).await?;
    assign(&mut spec.tcp_ports, &mut used, allocate)?;
    spec.materialize_tcp_ports();
    Ok(())
}

/// Confere as portas manuais e aloca as automáticas (`0`), atualizando `used`. Separado de
/// [`resolve_host_port`] (que lê o banco) para dar para testar com um alocador falso.
fn assign(
    routes: &mut [TcpPortRoute],
    used: &mut Vec<u16>,
    mut alloc: impl FnMut(&[u16]) -> Result<u16, String>,
) -> Result<(), String> {
    for r in routes.iter().filter(|r| r.host_port != AUTO_PORT) {
        if used.contains(&r.host_port) {
            return Err(format!(
                "porta externa {} já está reservada por outro serviço, por outra porta deste serviço ou pelo próprio rustploy",
                r.host_port
            ));
        }
        used.push(r.host_port);
    }
    for r in routes.iter_mut().filter(|r| r.host_port == AUTO_PORT) {
        let p = alloc(used)?;
        r.host_port = p;
        used.push(p);
    }
    Ok(())
}

/// Para cada porta TCP do spec: a porta do host e os destinos `ip:porta_do_container` (um por IP em
/// `ips`). É o que cada caminho de deploy registra no ingress.
pub fn tcp_targets(spec: &ServiceSpec, ips: &[String]) -> Vec<(u16, Vec<String>)> {
    spec.tcp_routes()
        .iter()
        .filter(|r| r.host_port != AUTO_PORT)
        .map(|r| {
            let container = r.container_port(spec.port);
            (
                r.host_port,
                ips.iter().map(|ip| format!("{ip}:{container}")).collect(),
            )
        })
        .collect()
}

/// Portas de host de um spec (as automáticas ainda não resolvidas ficam de fora).
pub fn host_ports(spec: &ServiceSpec) -> Vec<u16> {
    spec.tcp_routes()
        .iter()
        .map(|r| r.host_port)
        .filter(|p| *p != AUTO_PORT)
        .collect()
}

/// True se `port` ainda é reservada por algum serviço ≠ `exclude_id` (ou pelo
/// próprio daemon) — usado antes de um `deny` no firewall, para nunca fechar
/// uma porta que outro serviço compartilha.
pub async fn port_in_use_by_other(db: &Db, port: u16, exclude_id: Option<&str>) -> bool {
    match used_ports(db, exclude_id).await {
        Ok(used) => used.contains(&port),
        // Na dúvida (falha de DB), não mexe no firewall.
        Err(_) => true,
    }
}

/// Portas de host indisponíveis: as portas TCP de todos os serviços (exceto
/// `exclude_id`) + portas do próprio daemon (ingress e API).
async fn used_ports(db: &Db, exclude_id: Option<&str>) -> Result<Vec<u16>, String> {
    let cfg = RustployConfig::global();
    let mut used = vec![
        cfg.ingress.http_port,
        cfg.ingress.https_port,
        // A API também serve o webhook e o callback OAuth — uma porta só.
        cfg.api.port,
    ];
    let services = crate::store::services::list_all(db)
        .await
        .map_err(|e| format!("falha ao listar serviços para alocação de porta: {e}"))?;
    for s in services {
        if Some(s.id.as_str()) == exclude_id {
            continue;
        }
        used.extend(host_ports(&s.spec));
    }
    Ok(used)
}

/// Varre a faixa configurada e devolve a primeira porta que (a) nenhum serviço
/// reserva e (b) nenhum processo do SO ocupa (sondagem via bind descartado na
/// hora — cobre colisão com software alheio ao rustploy).
fn allocate(used: &[u16]) -> Result<u16, String> {
    let range = &RustployConfig::global().external_ports;
    for port in range.range_start..=range.range_end {
        if used.contains(&port) {
            continue;
        }
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
            continue;
        }
        return Ok(port);
    }
    Err(format!(
        "faixa de portas externas esgotada ({}-{}); ajuste [external_ports] na config",
        range.range_start, range.range_end
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rota(host: u16) -> TcpPortRoute {
        TcpPortRoute {
            host_port: host,
            container_port: None,
            label: None,
        }
    }

    /// Alocador falso: a primeira porta a partir de 30000 que ninguém usa.
    fn falso(used: &[u16]) -> Result<u16, String> {
        Ok((30000..).find(|p| !used.contains(p)).unwrap())
    }

    #[test]
    fn duas_automaticas_no_mesmo_servico_recebem_portas_diferentes() {
        let mut r = vec![rota(AUTO_PORT), rota(AUTO_PORT)];
        assign(&mut r, &mut vec![], falso).unwrap();
        assert_eq!((r[0].host_port, r[1].host_port), (30000, 30001));
    }

    #[test]
    fn automatica_nao_pega_a_manual_do_mesmo_servico() {
        let mut r = vec![rota(AUTO_PORT), rota(30000)];
        assign(&mut r, &mut vec![], falso).unwrap();
        assert_eq!(r[0].host_port, 30001);
    }

    #[test]
    fn porta_manual_repetida_e_recusada() {
        // no mesmo serviço
        assert!(assign(&mut [rota(20002), rota(20002)], &mut vec![], falso).is_err());
        // em outro serviço (ou do próprio daemon)
        assert!(assign(&mut [rota(20002)], &mut vec![20002], falso).is_err());
    }

    #[test]
    fn allocate_pula_portas_reservadas() {
        let range = &RustployConfig::global().external_ports;
        let first = range.range_start;
        let port = allocate(&[first, first + 1]).expect("deve alocar");
        assert!(port > first + 1);
        assert!(range.contains(port));
    }

    #[test]
    fn allocate_pula_porta_ocupada_no_so() {
        let range = &RustployConfig::global().external_ports;
        // Ocupa no SO a primeira porta *livre* da faixa (outra instância, como um
        // daemon de dev, pode já estar em portas iniciais) e confere que a
        // sondagem a pula.
        let (guard, held) = (range.range_start..=range.range_end)
            .find_map(|p| {
                std::net::TcpListener::bind(("0.0.0.0", p))
                    .ok()
                    .map(|l| (l, p))
            })
            .expect("nenhuma porta livre na faixa no ambiente de teste");
        let port = allocate(&[]).expect("deve alocar");
        assert_ne!(port, held);
        drop(guard);
    }
}
