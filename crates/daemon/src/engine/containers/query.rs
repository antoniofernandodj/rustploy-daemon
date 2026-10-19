//! Busca de containers por label, nome e prefixo, e o índice dos gerenciados.

use anyhow::Result;
use bollard::Docker;
use std::collections::HashMap;
use tracing::{debug, info, warn};

use super::*;

/// Staging da réplica `idx` deste deploy: no nome atual ou, para um deploy
/// iniciado antes da atualização do daemon, no legado. Sempre conferindo o
/// dono.
pub async fn find_staging(
    docker: &Docker,
    service_id: &str,
    service_name: &str,
    dep_short: &str,
    idx: u32,
) -> Result<Option<String>> {
    let atual = replica_staging_name(service_id, service_name, dep_short, idx);
    if let Some(id) = find_owned_by_name(docker, &atual, service_id).await? {
        return Ok(Some(id));
    }
    let legado = legacy_replica_staging_name(service_name, dep_short, idx);
    find_owned_by_name(docker, &legado, service_id).await
}

/// Índice de réplica de um container: o label `rustploy.replica` ou, em
/// container de antes do label, o sufixo `_r<i>` do nome (sem sufixo = 0).
pub(super) fn replica_index(labels: Option<&HashMap<String, String>>, name: &str) -> u32 {
    if let Some(i) = labels
        .and_then(|l| l.get("rustploy.replica"))
        .and_then(|v| v.parse().ok())
    {
        return i;
    }
    name.rsplit_once("_r")
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

/// Réplica live de um serviço Application, achada por label.
#[derive(Debug, Clone)]
pub struct LiveReplica {
    pub id: String,
    pub name: String,
    pub replica: u32,
    pub running: bool,
}

/// Todas as réplicas live (não-staging) de um serviço, **pelo label**
/// `rustploy.service_id` — não pelo nome. Acha tanto `rp_<id8>_<safe>` quanto
/// o legado `rp_<safe>`, e continua achando depois de um rename do serviço.
/// Ordenadas por réplica; em empate (legado + atual da mesma réplica, durante
/// a transição), a que está rodando vem primeiro.
pub async fn find_live_replicas(docker: &Docker, service_id: &str) -> Result<Vec<LiveReplica>> {
    use bollard::container::ListContainersOptions;
    let mut filters = HashMap::new();
    // Filtra só pela existência da chave: o valor pode estar no formato de
    // antes dos prefixos de ID (sem `svc_`), que `label_owned_by` aceita.
    filters.insert("label".to_string(), vec!["rustploy.service_id".to_string()]);
    let opts = ListContainersOptions {
        all: true,
        filters,
        ..Default::default()
    };
    let mut out: Vec<LiveReplica> = docker
        .list_containers(Some(opts))
        .await?
        .into_iter()
        .filter(|c| {
            c.labels
                .as_ref()
                .and_then(|l| l.get("rustploy.service_id"))
                .is_some_and(|o| label_owned_by(o, service_id))
        })
        .filter_map(|c| {
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())?;
            if is_staging_name(&name) {
                return None;
            }
            Some(LiveReplica {
                id: c.id?,
                replica: replica_index(c.labels.as_ref(), &name),
                running: c.state.as_deref() == Some("running"),
                name,
            })
        })
        .collect();
    out.sort_by_key(|r| (r.replica, !r.running));
    debug!(service_id = %service_id, count = out.len(), "réplicas live encontradas");
    Ok(out)
}

/// IPs, na rede `network`, das réplicas live `0..replicas` de um serviço — uma
/// por réplica, a rodando quando houver duas (transição legado → atual).
/// Réplica sem IP na rede (parada, ou fora dela) fica de fora.
pub async fn live_replica_ips(
    docker: &Docker,
    service_id: &str,
    network: &str,
    replicas: u32,
) -> Vec<String> {
    let lives = match find_live_replicas(docker, service_id).await {
        Ok(v) => v,
        Err(e) => {
            warn!(service_id = %service_id, error = %e, "falha ao listar réplicas live");
            return vec![];
        }
    };
    let mut ips = Vec::new();
    for i in 0..replicas {
        for live in lives.iter().filter(|r| r.replica == i) {
            if let Ok(ip) = get_container_ip(docker, &live.id, network).await {
                ips.push(ip);
                break;
            }
        }
    }
    ips
}

pub async fn find_all_by_service_id(docker: &Docker, service_id: &str) -> Result<Vec<String>> {
    use bollard::container::ListContainersOptions;
    debug!(
        service_id = %service_id,
        "buscando containers por service_id"
    );
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("rustploy.service_id={service_id}")],
    );
    let opts = ListContainersOptions {
        all: true,
        filters,
        ..Default::default()
    };
    let list = docker.list_containers(Some(opts)).await?;
    let ids: Vec<String> = list.into_iter().filter_map(|c| c.id).collect();

    debug!(
        service_id = %service_id,
        count = ids.len(),
        "containers encontrados"
    );
    Ok(ids)
}

/// Container gerenciado pelo rustploy, no formato leve que o GUI exibe
/// (id + nome + estado). Serializado direto no snapshot HTTP/JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ManagedContainer {
    /// ID completo do container (o GUI encurta para exibição).
    pub id: String,
    /// Nome do container (sem a barra inicial que o Docker devolve), ex. `rp_web`.
    pub name: String,
    /// Estado do container: `running`, `exited`, `created`, ...
    pub state: String,
}

/// Índice de containers do host resolvido numa única listagem, com duas chaves:
/// por `rustploy.service_id` (serviços normais — live/staging/réplicas) e por
/// `com.docker.compose.project` (serviços Compose, cujos containers são criados
/// pelo `docker compose` e **não** carregam os labels `rustploy.*`). O snapshot
/// consulta com [`ContainerIndex::for_service`], que tenta o service_id e cai
/// para o nome de projeto Compose.
#[derive(Default)]
pub struct ContainerIndex {
    by_service_id: HashMap<String, Vec<ManagedContainer>>,
    by_compose_project: HashMap<String, Vec<ManagedContainer>>,
}

impl ContainerIndex {
    /// Containers de um serviço: pelos labels `rustploy.service_id`, ou — quando
    /// vazio (serviço Compose) — pelo `com.docker.compose.project` gravado do
    /// serviço (`Service::compose_project_name`).
    pub fn for_service(&self, service: &shared::Service) -> Vec<ManagedContainer> {
        if let Some(v) = self.by_service_id.get(&service.id) {
            return v.clone();
        }
        self.by_compose_project
            .get(&service.compose_project_name())
            .cloned()
            .unwrap_or_default()
    }
}

/// Stacks Compose que existem no host (`com.docker.compose.project`, com os
/// containers vivos ou parados), cada uma com o conjunto de chaves de serviço
/// que ela tem (`com.docker.compose.service`). Alimenta a migração que grava o
/// nome da stack de cada serviço (`db::services::backfill_compose_projects`):
/// o container de uma stack não leva label do serviço do rustploy, então as
/// chaves de serviço são o único jeito de ligar stack e serviço sem depender do
/// prefixo do nome (igual para tudo que foi criado no mesmo segundo).
pub async fn list_compose_projects(
    docker: &Docker,
) -> Result<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>> {
    use bollard::container::ListContainersOptions;
    let list = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        }))
        .await?;
    let mut out: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    for c in list {
        let labels = c.labels.unwrap_or_default();
        if let Some(project) = labels.get("com.docker.compose.project") {
            let keys = out.entry(project.clone()).or_default();
            if let Some(svc) = labels.get("com.docker.compose.service") {
                keys.insert(svc.clone());
            }
        }
    }
    Ok(out)
}

/// Lista **todos** os containers do host numa única chamada e os indexa por
/// service_id e por projeto Compose (ver [`ContainerIndex`]). Erros de Docker
/// degradam para um índice vazio (o snapshot segue sem a informação).
pub async fn index_containers(docker: &Docker) -> ContainerIndex {
    use bollard::container::ListContainersOptions;
    let opts = ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    };
    let list = match docker.list_containers(Some(opts)).await {
        Ok(l) => l,
        Err(e) => {
            warn!(
                error = %e,
                "containers::index_containers: falha ao listar containers"
            );
            return ContainerIndex::default();
        }
    };
    let mut idx = ContainerIndex::default();
    for c in list {
        let labels = c.labels.clone().unwrap_or_default();
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();

        let mc = ManagedContainer {
            id: c.id.unwrap_or_default(),
            name,
            state: c.state.unwrap_or_default(),
        };
        if let Some(service_id) = labels.get("rustploy.service_id") {
            idx.by_service_id
                .entry(service_id.clone())
                .or_default()
                .push(mc);
        } else if let Some(project) = labels.get("com.docker.compose.project") {
            idx.by_compose_project
                .entry(project.clone())
                .or_default()
                .push(mc);
        }
    }
    idx
}

/// Returns container IDs for a service excluding those from the given deployment.
pub async fn find_old_containers(
    docker: &Docker,
    service_id: &str,
    exclude_deployment_id: &str,
) -> Result<Vec<String>> {
    use bollard::container::ListContainersOptions;
    debug!(
        service_id = %service_id,
        "containers::find_old_containers: buscando"
    );
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("rustploy.service_id={service_id}")],
    );
    let opts = ListContainersOptions {
        all: true,
        filters,
        ..Default::default()
    };
    let list = docker.list_containers(Some(opts)).await?;
    let ids: Vec<String> = list
        .into_iter()
        .filter(|c| {
            let dep = c
                .labels
                .as_ref()
                .and_then(|l| l.get("rustploy.deployment_id"))
                .map(|s| s.as_str())
                .unwrap_or("");
            dep != exclude_deployment_id
        })
        .filter_map(|c| c.id)
        .collect();

    debug!(
        service_id = %service_id,
        count = ids.len(),
        "containers antigos encontrados"
    );
    Ok(ids)
}

/// TODO: sem uso desde a checagem de dono. **Não** usar para containers de
/// serviço (`rp_<safe>`…): o nome não diz de quem o container é — use
/// [`find_owned_by_name`]. Mantida para buscas de container fora desse caso.
#[allow(dead_code)]
pub async fn find_by_name(docker: &Docker, name: &str) -> Result<Option<String>> {
    use bollard::container::ListContainersOptions;
    debug!(name = %name, "buscando container por nome");
    let mut filters = HashMap::new();
    filters.insert("name".to_string(), vec![format!("^/{name}$")]);
    let opts = ListContainersOptions {
        all: true,
        filters,
        ..Default::default()
    };
    let containers = docker.list_containers(Some(opts)).await?;
    let found = containers.into_iter().next().and_then(|c| c.id);
    match &found {
        Some(id) => {
            debug!(
                name = %name,
                container_id = %id,
                "container encontrado por nome"
            )
        }
        None => debug!(
            name = %name,
            "container não encontrado"
        ),
    }
    Ok(found)
}

/// Quem ocupa o nome `name` no host: `None` se não há container com esse
/// nome; senão `(container_id, rustploy.service_id)`, com o label `None` num
/// container sem ele (não criado pelo rustploy, ou antigo demais).
pub async fn name_owner(docker: &Docker, name: &str) -> Result<Option<(String, Option<String>)>> {
    use bollard::container::ListContainersOptions;
    let mut filters = HashMap::new();
    filters.insert("name".to_string(), vec![format!("^/{name}$")]);
    let opts = ListContainersOptions {
        all: true,
        filters,
        ..Default::default()
    };
    let found = docker.list_containers(Some(opts)).await?.into_iter().next();
    Ok(found.and_then(|c| {
        let owner = c
            .labels
            .as_ref()
            .and_then(|l| l.get("rustploy.service_id"))
            .cloned();
        c.id.map(|id| (id, owner))
    }))
}

/// Como [`find_by_name`], mas só devolve o container se ele pertencer a
/// `service_id` (label `rustploy.service_id`).
///
/// Nome de container é único no **host**, mas nome de serviço só é único no
/// **projeto**: dois projetos com um serviço `api` disputam o nome `rp_api`.
/// Achar "o live deste serviço" só pelo nome faz o deploy de um parar e
/// remover o container do outro. Toda busca por `replica_live_name` /
/// `replica_staging_name` passa por aqui. Ver
/// `docs/plano-colisao-nome-container.md`.
pub async fn find_owned_by_name(
    docker: &Docker,
    name: &str,
    service_id: &str,
) -> Result<Option<String>> {
    match name_owner(docker, name).await? {
        Some((id, Some(owner))) if label_owned_by(&owner, service_id) => Ok(Some(id)),
        Some((id, owner)) => {
            warn!(
                name = %name,
                container_id = %id,
                owner = ?owner,
                service_id = %service_id,
                "container com o nome esperado pertence a outro serviço (ou a nenhum) — ignorado"
            );
            Ok(None)
        }
        None => Ok(None),
    }
}

/// Containers **rodando** cujo nome começa com `prefix`, como `(id, nome)`,
/// ordenados por nome.
///
/// A ordenação não é cosmética: é o que torna a escolha do alvo de ingress
/// determinística. A ordem que o Docker devolve não é estável, e uma stack
/// Compose que troca de container-alvo entre dois boots é exatamente o bug que
/// isto existe para não repetir.
pub async fn list_running_by_prefix(
    docker: &Docker,
    prefix: &str,
) -> Result<Vec<(String, String)>> {
    use bollard::container::ListContainersOptions;
    let mut filters = HashMap::new();
    filters.insert("name".to_string(), vec![format!("^/{prefix}")]);
    let opts = ListContainersOptions {
        all: false, // só running: container parado não serve de backend
        filters,
        ..Default::default()
    };
    let mut out: Vec<(String, String)> = docker
        .list_containers(Some(opts))
        .await?
        .into_iter()
        .filter_map(|c| {
            let id = c.id?;
            let name = c
                .names
                .unwrap_or_default()
                .into_iter()
                .next()?
                .trim_start_matches('/')
                .to_string();
            Some((id, name))
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

/// Portas TCP que o container declara expor (`EXPOSE` da imagem + `expose:`/
/// `ports:` do compose).
pub(super) async fn exposed_tcp_ports(docker: &Docker, container_id: &str) -> Vec<u16> {
    let Ok(info) = inspect(docker, container_id).await else {
        return Vec::new();
    };
    let mut out: Vec<u16> = info
        .config
        .as_ref()
        .and_then(|c| c.exposed_ports.as_ref())
        .map(|ports| {
            ports
                .keys()
                // chave no formato "8000/tcp"
                .filter_map(|k| {
                    let (num, proto) = k.split_once('/').unwrap_or((k.as_str(), "tcp"));
                    (proto == "tcp").then(|| num.parse::<u16>().ok()).flatten()
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_unstable();
    out
}

/// Índice do candidato cujo nome é o do serviço `svc` dentro do compose.
///
/// O Compose nomeia `<projeto>-<serviço>-<réplica>`; aceita também o nome sem
/// sufixo de réplica, que é o que aparece com `container_name:` fixo.
pub(super) fn match_ingress_service(
    candidates: &[(String, String)],
    prefix: &str,
    svc: &str,
) -> Option<usize> {
    let exact = format!("{prefix}{svc}");
    let replica = format!("{prefix}{svc}-");
    candidates
        .iter()
        .position(|(_, n)| *n == exact || n.starts_with(&replica))
}

/// Índice do primeiro candidato que expõe alguma das portas pedidas.
///
/// A ordem de `want_ports` manda: a primeira porta pedida que alguém expuser
/// decide, para que um serviço com dois domínios em portas diferentes caia
/// sempre no mesmo container em vez de alternar.
pub(super) fn match_exposed_port(exposed: &[Vec<u16>], want_ports: &[u16]) -> Option<usize> {
    want_ports
        .iter()
        .find_map(|want| exposed.iter().position(|ports| ports.contains(want)))
}

/// Escolhe qual container de uma stack Compose recebe o tráfego do ingress.
///
/// Uma stack tem N containers e só um atende o domínio — num Supabase
/// self-hosted são nove, e quem serve a porta 8000 é o `kong`. Até 2026-08-28
/// isto era `find_by_prefix`, que "pega o primeiro que encontrar": a rota caía
/// num container arbitrário da stack e o domínio respondia **502**.
///
/// Ordem de decisão:
/// 1. `ingress_service` do `ComposeSource`, quando preenchido — o compose
///    nomeia os containers `<projeto>-<serviço>-<n>`;
/// 2. o container que expõe alguma das `want_ports` (as portas que os domínios
///    do serviço pedem) — resolve o caso comum sem configuração nenhuma;
/// 3. o primeiro por nome, com `warn` no log — último recurso, para não deixar
///    o serviço sem rota alguma.
pub async fn find_compose_ingress_container(
    docker: &Docker,
    project_name: &str,
    ingress_service: Option<&str>,
    want_ports: &[u16],
) -> Result<Option<String>> {
    let prefix = format!("{project_name}-");
    let candidates = list_running_by_prefix(docker, &prefix).await?;
    if candidates.is_empty() {
        return Ok(None);
    }

    // 1. Alvo declarado pelo usuário.
    if let Some(svc) = ingress_service.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(i) = match_ingress_service(&candidates, &prefix, svc) {
            let (id, name) = &candidates[i];
            info!(container = %name, ingress_service = %svc, "compose ingress: alvo declarado");
            return Ok(Some(id.clone()));
        }
        // Não abortamos: um nome errado no spec não deve derrubar a stack
        // inteira. Mas o log tem de dizer por que a escolha não foi a pedida.
        warn!(
            ingress_service = %svc,
            project = %project_name,
            candidatos = ?candidates.iter().map(|(_, n)| n).collect::<Vec<_>>(),
            "compose ingress: `ingress_service` não existe na stack — caindo para a descoberta por porta"
        );
    }

    // 2. Quem expõe a porta que o domínio pede.
    if !want_ports.is_empty() {
        let mut portas_por_container: Vec<(String, String, Vec<u16>)> =
            Vec::with_capacity(candidates.len());
        for (id, name) in &candidates {
            let ports = exposed_tcp_ports(docker, id).await;
            portas_por_container.push((id.clone(), name.clone(), ports));
        }
        let so_portas: Vec<Vec<u16>> = portas_por_container
            .iter()
            .map(|(_, _, p)| p.clone())
            .collect();
        if let Some(i) = match_exposed_port(&so_portas, want_ports) {
            let (id, name, _) = &portas_por_container[i];
            info!(container = %name, "compose ingress: alvo por porta exposta");
            return Ok(Some(id.clone()));
        }
        warn!(
            project = %project_name,
            ?want_ports,
            candidatos = ?portas_por_container
                .iter()
                .map(|(_, n, p)| format!("{n}{p:?}"))
                .collect::<Vec<_>>(),
            "compose ingress: nenhum container expõe a porta pedida — usando o primeiro por nome"
        );
    }

    // 3. Último recurso, determinístico.
    Ok(candidates.into_iter().next().map(|(id, _)| id))
}

#[cfg(test)]
mod tests_ingress {
    use super::*;

    fn cands(nomes: &[&str]) -> Vec<(String, String)> {
        nomes
            .iter()
            .map(|n| (format!("id_{n}"), n.to_string()))
            .collect()
    }

    /// A stack real que motivou tudo isto: nove containers de um Supabase
    /// self-hosted, e só o `kong` atende a porta 8000 do domínio.
    const STACK: &[&str] = &[
        "rp_01m11z0g_db-auth-1",
        "rp_01m11z0g_db-db-1",
        "rp_01m11z0g_db-functions-1",
        "rp_01m11z0g_db-imgproxy-1",
        "rp_01m11z0g_db-kong-1",
        "rp_01m11z0g_db-meta-1",
        "rp_01m11z0g_db-rest-1",
        "rp_01m11z0g_db-storage-1",
        "rp_01m11z0g_db-supavisor-1",
    ];

    #[test]
    fn alvo_declarado_acha_o_container_da_replica() {
        let c = cands(STACK);
        let i = match_ingress_service(&c, "rp_01m11z0g_db-", "kong").unwrap();
        assert_eq!(c[i].1, "rp_01m11z0g_db-kong-1");
    }

    #[test]
    fn alvo_declarado_aceita_nome_sem_sufixo_de_replica() {
        let c = cands(&["rp_x-proxy", "rp_x-app-1"]);
        let i = match_ingress_service(&c, "rp_x-", "proxy").unwrap();
        assert_eq!(c[i].1, "rp_x-proxy");
    }

    /// Um `ingress_service` errado não pode casar por acidente com o prefixo
    /// de outro serviço: `db` não é `db-db`.
    #[test]
    fn alvo_declarado_inexistente_nao_casa() {
        let c = cands(&["rp_x-kong-1", "rp_x-storage-1"]);
        assert!(match_ingress_service(&c, "rp_x-", "nginx").is_none());
    }

    /// O caso do 502: sem alvo declarado, quem expõe a porta do domínio vence,
    /// mesmo estando no meio da lista.
    #[test]
    fn porta_do_dominio_escolhe_o_gateway_e_nao_o_primeiro() {
        // mesma ordem de STACK; só o índice 4 (kong) expõe 8000
        let exposed = vec![
            vec![9999],       // auth
            vec![5432],       // db
            vec![9000],       // functions
            vec![8080],       // imgproxy
            vec![8000, 8443], // kong
            vec![8080],       // meta
            vec![3000],       // rest
            vec![5000],       // storage
            vec![4000, 5452], // supavisor
        ];
        assert_eq!(match_exposed_port(&exposed, &[8000]), Some(4));
    }

    #[test]
    fn sem_ninguem_expondo_a_porta_nao_ha_escolha_por_porta() {
        let exposed = vec![vec![5432], vec![5000]];
        assert_eq!(match_exposed_port(&exposed, &[8000]), None);
    }

    /// Duas portas pedidas: manda a ordem de `want_ports`, não a dos containers.
    #[test]
    fn a_primeira_porta_pedida_decide() {
        let exposed = vec![vec![443], vec![8000]];
        assert_eq!(match_exposed_port(&exposed, &[8000, 443]), Some(1));
        assert_eq!(match_exposed_port(&exposed, &[443, 8000]), Some(0));
    }
}

#[cfg(test)]
mod tests_owner {
    use super::*;
    use bollard::container::{Config, CreateContainerOptions};

    const SVC: &str = "svc_01JABCDEFGHJKMNPQRSTVWXYZ0";

    #[test]
    fn nomes_atuais_tem_o_id() {
        assert_eq!(replica_live_name(SVC, "api", 0), "rp_stvwxyz0_api");
        assert_eq!(replica_live_name(SVC, "api", 2), "rp_stvwxyz0_api_r2");
        assert_eq!(
            replica_staging_name(SVC, "api", "01JQQQQQ", 1),
            "rp_stvwxyz0_api_staging_01JQQQQQ_r1"
        );
        assert_eq!(
            legacy_replica_staging_name("api", "01JQQQQQ", 0),
            "rp_api_staging_01JQQQQQ"
        );
    }

    #[test]
    fn staging_reconhecido_nos_dois_formatos() {
        assert!(is_staging_name("rp_stvwxyz0_api_staging_01JQQQQQ"));
        assert!(is_staging_name("rp_stvwxyz0_api_staging_01JQQQQQ_r3"));
        assert!(is_staging_name("rp_api_staging_01JQQQQQ"));
        assert!(!is_staging_name("rp_stvwxyz0_api"));
        assert!(!is_staging_name("rp_api_r1"));
        // serviço cujo nome contém "staging": o live não é staging
        assert!(!is_staging_name("rp_stvwxyz0_staging"));
        assert!(!is_staging_name("rp_stvwxyz0_app_staging_area_de_testes"));
        assert!(!is_staging_name("rp_stvwxyz0_app_staging_prod"));
        assert!(!is_staging_name("rp_stvwxyz0_app_staging_prod_r1"));
    }

    #[test]
    fn indice_de_replica_pelo_label_ou_pelo_nome() {
        let mut l = HashMap::new();
        l.insert("rustploy.replica".to_string(), "3".to_string());
        assert_eq!(replica_index(Some(&l), "rp_stvwxyz0_api"), 3);
        assert_eq!(replica_index(None, "rp_api_r2"), 2);
        assert_eq!(replica_index(None, "rp_api"), 0);
        assert_eq!(replica_index(None, "rp_x_redis"), 0);
    }

    #[test]
    fn label_do_proprio_servico() {
        assert!(label_owned_by("svc_01JABC", "svc_01JABC"));
    }

    #[test]
    fn label_de_antes_dos_prefixos_de_id() {
        assert!(label_owned_by("01JABC", "svc_01JABC"));
    }

    #[test]
    fn label_de_outro_servico() {
        assert!(!label_owned_by("svc_01JXYZ", "svc_01JABC"));
        assert!(!label_owned_by("01JXYZ", "svc_01JABC"));
        assert!(!label_owned_by("", "svc_01JABC"));
    }

    /// Contra um Docker de verdade: o `rp_<safe>` de um serviço homônimo de
    /// outro projeto não pode ser achado como se fosse o nosso.
    /// `cargo test -p rustploy tests_owner -- --ignored` (precisa de Docker e
    /// de uma imagem local qualquer em `RP_TEST_IMAGE`, padrão `redis:6.2-alpine`).
    #[tokio::test]
    #[ignore]
    async fn nome_ocupado_por_outro_servico_nao_e_achado() {
        let docker = Docker::connect_with_local_defaults().expect("docker");
        let image = std::env::var("RP_TEST_IMAGE").unwrap_or_else(|_| "redis:6.2-alpine".into());
        let name = format!("rp_teste_dono_{}", std::process::id());

        let mut labels = HashMap::new();
        labels.insert("rustploy.service_id".to_string(), "svc_OUTRO".to_string());
        let id = docker
            .create_container(
                Some(CreateContainerOptions {
                    name: name.clone(),
                    platform: None,
                }),
                Config {
                    image: Some(image),
                    labels: Some(labels),
                    ..Default::default()
                },
            )
            .await
            .expect("create")
            .id;

        let meu = find_owned_by_name(&docker, &name, "svc_MEU").await;
        let dele = find_owned_by_name(&docker, &name, "svc_OUTRO").await;
        let dono = name_owner(&docker, &name).await;
        let _ = remove(&docker, &id).await;

        assert_eq!(
            meu.unwrap(),
            None,
            "container de outro serviço não pode ser o nosso"
        );
        assert_eq!(dele.unwrap(), Some(id.clone()));
        assert_eq!(dono.unwrap(), Some((id, Some("svc_OUTRO".to_string()))));
        assert_eq!(name_owner(&docker, &name).await.unwrap(), None);
    }
}
