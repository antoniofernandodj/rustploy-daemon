//! Sugestões de porta do container para as portas TCP de um serviço (`Command::ServicePortSuggestions`).
//! Plano: `docs/plano-multiplas-portas-tcp.md`, Fase 3.
//!
//! Ler só o compose não basta (o do RustFS não declara `ports:`, só `RUSTFS_CONSOLE_ADDRESS`), então
//! quatro fontes são juntadas:
//! 1. o que o container **escuta agora** (`/proc/net/tcp` e `tcp6`, lidos de dentro dele);
//! 2. o que o container/imagem **declara** (`Config.ExposedPorts`: o `EXPOSE` da imagem + o `expose:`);
//! 3. o que o **compose** declara (`ports:` e `expose:`);
//! 4. uma **tabela por produto**, que dá nome às portas conhecidas e marca as internas.
//!
//! Nenhuma fonte é obrigatória: a que falhar (container parado, imagem sem `cat`) some sem erro.

use shared::{PortSuggestion, Service, ServiceSource};
use std::collections::BTreeMap;

const LISTENING: &str = "listening";
const IMAGE: &str = "image";
const COMPOSE: &str = "compose";
const PRODUCT: &str = "product";

/// Portas conhecidas por produto: (porta, nome, interna).
fn product_ports(product: &str) -> &'static [(u16, &'static str, bool)] {
    match product {
        "rustfs" | "minio" => &[(9000, "API S3", false), (9001, "console", false)],
        "rabbitmq" => &[
            (5672, "AMQP", false),
            (15672, "painel", false),
            (5671, "AMQPS", false),
            (15692, "métricas", false),
            (25672, "cluster", true),
            (4369, "epmd (cluster)", true),
            (15691, "métricas TLS", false),
        ],
        "postgres" => &[(5432, "Postgres", false)],
        "mysql" | "mariadb" => &[(3306, "MySQL", false)],
        "mongodb" => &[(27017, "MongoDB", false)],
        "redis" => &[(6379, "Redis", false)],
        "kafka" => &[(9092, "Kafka", false), (9093, "controller", true)],
        "nats" => &[
            (4222, "NATS", false),
            (8222, "monitoramento", false),
            (6222, "cluster", true),
        ],
        _ => &[],
    }
}

/// Portas em LISTEN de um `/proc/net/tcp` (ou `tcp6`), **fora do loopback**: o ingress chega pelo IP do
/// container, então quem escuta só em `127.0.0.1`/`::1` não é alcançável.
fn listening_ports(proc_net_tcp: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for line in proc_net_tcp.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st ...; 0A = LISTEN
        if cols.len() < 4 || cols[3] != "0A" {
            continue;
        }
        let Some((addr, port)) = cols[1].split_once(':') else {
            continue;
        };
        let loopback = addr == "0100007F" || addr == "00000000000000000000000001000000";
        if let (false, Ok(p)) = (loopback, u16::from_str_radix(port, 16))
            && !out.contains(&p)
        {
            out.push(p);
        }
    }
    out
}

/// Portas do container que o compose declara (`ports:` e `expose:`), no serviço `only` (o que recebe o
/// ingress) ou em todos.
fn compose_ports(content: &str, only: Option<&str>) -> Vec<u16> {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(content) else {
        return Vec::new();
    };
    let Some(services) = doc.get("services").and_then(|s| s.as_mapping()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, svc) in services {
        if only.is_some_and(|o| name.as_str() != Some(o)) {
            continue;
        }
        for key in ["ports", "expose"] {
            for item in svc
                .get(key)
                .and_then(|v| v.as_sequence())
                .into_iter()
                .flatten()
            {
                if let Some(p) = container_port_of(item)
                    && !out.contains(&p)
                {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// A porta do container de um item de `ports:`/`expose:`: `80`, `"8080:80"`, `"127.0.0.1:8080:80/tcp"`,
/// ou a forma longa `{ target: 80 }`. Faixas (`"8000-8010"`) ficam de fora.
fn container_port_of(item: &serde_yaml::Value) -> Option<u16> {
    if let Some(n) = item.as_u64() {
        return u16::try_from(n).ok();
    }
    if let Some(t) = item.get("target") {
        return t.as_u64().and_then(|n| u16::try_from(n).ok());
    }
    let s = item.as_str()?;
    let last = s.rsplit(':').next()?;
    last.split('/').next()?.parse().ok()
}

/// Junta as fontes: uma sugestão por porta, com o nome da tabela, as origens e "interna". Ordem: externas
/// antes das internas; dentro de cada grupo, as que o container escuta agora primeiro; depois por porta.
/// As portas em `skip` (já configuradas) ficam de fora.
fn merge(
    product: Option<&str>,
    listening: &[u16],
    image: &[u16],
    compose: &[u16],
    skip: &[u16],
) -> Vec<PortSuggestion> {
    let mut by_port: BTreeMap<u16, PortSuggestion> = BTreeMap::new();
    let mut add = |port: u16, source: &str| {
        let s = by_port.entry(port).or_insert_with(|| PortSuggestion {
            port,
            label: None,
            sources: Vec::new(),
            internal: false,
        });
        if !s.sources.iter().any(|x| x == source) {
            s.sources.push(source.to_string());
        }
    };
    for p in listening {
        add(*p, LISTENING);
    }
    for p in image {
        add(*p, IMAGE);
    }
    for p in compose {
        add(*p, COMPOSE);
    }
    for (p, _, _) in product.map(product_ports).unwrap_or_default() {
        add(*p, PRODUCT);
    }
    for (p, label, internal) in product.map(product_ports).unwrap_or_default() {
        if let Some(s) = by_port.get_mut(p) {
            s.label = Some((*label).to_string());
            s.internal = *internal;
        }
    }
    let mut out: Vec<PortSuggestion> = by_port
        .into_values()
        .filter(|s| !skip.contains(&s.port))
        .collect();
    out.sort_by_key(|s| {
        (
            s.internal,
            !s.sources.iter().any(|x| x == LISTENING),
            s.port,
        )
    });
    out
}

/// Roda o CLI do motor e devolve a saída, ou `None` se falhar (fonte indisponível, sem erro).
async fn engine_out(args: &[&str]) -> Option<String> {
    let out = crate::engine::cli().args(args).output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `Command::ServicePortSuggestions`.
pub async fn suggest(svc: &Service) -> Vec<PortSuggestion> {
    let (mut listening, mut image) = (Vec::new(), Vec::new());
    if let Some(cid) = svc.live_container_id.as_deref() {
        // `cat` pode não existir na imagem (distroless): aí esta fonte some.
        if let Some(text) =
            engine_out(&["exec", cid, "cat", "/proc/net/tcp", "/proc/net/tcp6"]).await
        {
            listening = listening_ports(&text);
        }
        if let Some(json) =
            engine_out(&["inspect", "--format", "{{json .Config.ExposedPorts}}", cid]).await
        {
            image = exposed_ports(&json);
        }
    }
    let compose = match &svc.spec.source {
        ServiceSource::Compose(c) => compose_ports(&c.content, c.ingress_service.as_deref()),
        _ => Vec::new(),
    };
    let configured: Vec<u16> = svc
        .spec
        .tcp_routes()
        .iter()
        .map(|r| r.container_port(svc.spec.port))
        .collect();
    merge(
        svc.spec.product.as_deref(),
        &listening,
        &image,
        &compose,
        &configured,
    )
}

/// Portas TCP de um `{"9000/tcp":{},"53/udp":{}}` (o `Config.ExposedPorts` do motor).
fn exposed_ports(json: &str) -> Vec<u16> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(json.trim())
    else {
        return Vec::new();
    };
    map.keys()
        .filter_map(|k| {
            let (port, proto) = k.split_once('/').unwrap_or((k.as_str(), "tcp"));
            (proto == "tcp").then(|| port.parse().ok()).flatten()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trecho real de um RustFS: escuta 9000 e 9001 em todas as interfaces, e algo só no loopback.
    const PROC_TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:2328 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000 100 0 0 10 0
   1: 00000000:2329 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1 0000 100 0 0 10 0
   2: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 3 1 0000 100 0 0 10 0
   3: 0400590A:2328 0100590A:9C4E 01 00000000:00000000 00:00000000 00000000     0        0 4 1 0000 100 0 0 10 0
";

    #[test]
    fn escutando_fora_do_loopback_e_so_em_listen() {
        // 2328 = 9000, 2329 = 9001; 1F90 (8080) só no loopback; a linha 3 é uma conexão, não LISTEN
        assert_eq!(listening_ports(PROC_TCP), vec![9000, 9001]);
    }

    #[test]
    fn compose_le_ports_e_expose_em_todas_as_formas() {
        let c = "services:\n  web:\n    image: x\n    ports:\n      - 80\n      - \"8080:8000\"\n      - \"127.0.0.1:9443:443/tcp\"\n      - target: 5000\n        published: 5000\n      - \"7000-7010\"\n    expose:\n      - 6000\n  outro:\n    image: y\n    expose: [\"7777\"]\n";
        assert_eq!(
            compose_ports(c, Some("web")),
            vec![80, 8000, 443, 5000, 6000]
        );
        assert!(compose_ports(c, None).contains(&7777));
        assert!(compose_ports("isto não é yaml: [", None).is_empty());
    }

    #[test]
    fn exposed_ports_so_tcp() {
        assert_eq!(exposed_ports(r#"{"9000/tcp":{},"53/udp":{}}"#), vec![9000]);
        assert!(exposed_ports("null").is_empty());
    }

    /// O caso que motivou a feature: o console do RustFS (9001) aparece com nome, e a 9000 já
    /// configurada sai da lista.
    #[test]
    fn rustfs_sugere_o_console_com_nome() {
        let s = merge(Some("rustfs"), &[9000, 9001], &[], &[], &[9000]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].port, 9001);
        assert_eq!(s[0].label.as_deref(), Some("console"));
        assert_eq!(s[0].sources, vec!["listening", "product"]);
    }

    #[test]
    fn internas_vao_para_o_fim_e_escutando_vem_primeiro() {
        let s = merge(Some("rabbitmq"), &[5672, 15672, 25672], &[4369], &[], &[]);
        let portas: Vec<(u16, bool)> = s.iter().map(|x| (x.port, x.internal)).collect();
        // externas escutando, externas só da tabela, depois as internas (a que escuta primeiro)
        assert_eq!(portas[..2], [(5672, false), (15672, false)]);
        assert!(
            portas.ends_with(&[(25672, true), (4369, true)]),
            "{portas:?}"
        );
        assert_eq!(
            s.iter().find(|x| x.port == 15672).unwrap().label.as_deref(),
            Some("painel")
        );
    }

    #[test]
    fn servico_sem_produto_usa_so_o_que_achou() {
        let s = merge(None, &[3000], &[3000, 9229], &[], &[]);
        assert_eq!(
            s.iter().map(|x| x.port).collect::<Vec<_>>(),
            vec![3000, 9229]
        );
        assert!(s.iter().all(|x| x.label.is_none()));
    }
}
