//! O snapshot do painel (o `kind:"snapshot"` do SSE `/api/events` e o `Command::Snapshot`):
//! calculado **uma vez** para todos os clientes e dividido em **seções**.
//!
//! Antes, cada conexão SSE montava o snapshot inteiro a cada 2 s — ~15 `dispatch`, listas do
//! Docker, do registry, auditoria de infra — e o mandava inteiro, mudasse algo ou não. Com a GUI e
//! duas abas da webui eram três cálculos e três cópias por tick, e cada cliente decodificava tudo
//! de novo. Agora:
//!
//! - o [`SnapshotHub`] calcula as seções num laço único (só enquanto houver alguém ouvindo) e
//!   publica num `watch`; as conexões só repassam;
//! - cada seção é um texto JSON; a que não mudou reaproveita o mesmo `Arc<str>` do tick anterior,
//!   e o hub nem publica se nada mudou;
//! - um cliente que pede `GET /api/events?patch=1` recebe o snapshot inteiro ao conectar e, depois,
//!   só as seções que mudaram (`kind:"snapshot_patch"`); sem o parâmetro (clientes antigos) segue
//!   recebendo o snapshot inteiro a cada mudança, como sempre.
//!
//! Plano: `docs/arquivo/estudo-consumo-de-ram.md`, R-1.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::api::dispatch::dispatch;
use crate::bootstrap::state::AppState;
use crate::protocol::{Command, Response as RpResponse};

/// De quanto em quanto tempo o laço recalcula o snapshot.
pub(crate) const TICK: Duration = Duration::from_secs(2);

/// Um snapshot calculado: as seções na ordem fixa em que o JSON as escreve.
pub(crate) struct Snapshot {
    sections: Vec<(&'static str, Arc<str>)>,
    /// O JSON inteiro (`{"kind":"snapshot",…}`), montado só se alguém pedir — os clientes que
    /// recebem patches nunca pedem, e os antigos dividem a mesma cópia.
    full: OnceLock<String>,
}

impl Snapshot {
    fn get(&self, name: &str) -> Option<&Arc<str>> {
        self.sections
            .iter()
            .find_map(|(k, v)| (*k == name).then_some(v))
    }

    /// O snapshot inteiro, como o SSE sempre mandou.
    pub(crate) fn full_json(&self) -> &str {
        self.full.get_or_init(|| {
            let len = self
                .sections
                .iter()
                .map(|(k, v)| k.len() + v.len() + 4)
                .sum::<usize>();
            let mut out = String::with_capacity(len + 24);
            out.push_str(r#"{"kind":"snapshot""#);
            for (k, v) in &self.sections {
                push_field(&mut out, k, v);
            }
            out.push('}');
            out
        })
    }

    /// Só o que mudou desde `prev`: `{"kind":"snapshot_patch", <seções novas ou trocadas>,
    /// "removed":[<seções que sumiram>]}`. `None` quando nada mudou.
    pub(crate) fn patch_json(&self, prev: &Snapshot) -> Option<String> {
        let mut out = String::from(r#"{"kind":"snapshot_patch""#);
        let mut any = false;
        for (k, v) in &self.sections {
            // A seção que não mudou é o MESMO `Arc` do tick anterior (ver `SnapshotHub::refresh`).
            if prev.get(k).is_some_and(|old| Arc::ptr_eq(old, v)) {
                continue;
            }
            push_field(&mut out, k, v);
            any = true;
        }
        let removed: Vec<&str> = prev
            .sections
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| self.get(k).is_none())
            .collect();
        if !removed.is_empty() {
            out.push_str(r#","removed":"#);
            out.push_str(&serde_json::to_string(&removed).unwrap_or_else(|_| "[]".into()));
            any = true;
        }
        out.push('}');
        any.then_some(out)
    }
}

fn push_field(out: &mut String, key: &str, raw_json: &str) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":");
    out.push_str(raw_json);
}

/// O produtor único do snapshot. Vive no `AppState`.
pub struct SnapshotHub {
    tx: watch::Sender<Option<Arc<Snapshot>>>,
    /// Quando o último cálculo terminou — um cliente que conecta logo depois reaproveita.
    computed_at: Mutex<Option<Instant>>,
    /// Um cálculo por vez: o laço e um `Command::Snapshot` que chegam juntos não duplicam o trabalho.
    computing: tokio::sync::Mutex<()>,
    producer_started: AtomicBool,
}

impl SnapshotHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tx: watch::channel(None).0,
            computed_at: Mutex::new(None),
            computing: tokio::sync::Mutex::new(()),
            producer_started: AtomicBool::new(false),
        })
    }

    /// Calcula agora, publica para todos os assinantes se algo mudou e devolve o atual.
    pub(crate) async fn refresh(&self, state: &AppState) -> Arc<Snapshot> {
        let _one_at_a_time = self.computing.lock().await;
        let fresh = build_sections(state).await;
        let prev = self.tx.borrow().clone();

        let mut changed = prev
            .as_ref()
            .is_none_or(|p| p.sections.len() != fresh.len());
        let sections: Vec<(&'static str, Arc<str>)> = fresh
            .into_iter()
            .map(|(k, text)| match prev.as_ref().and_then(|p| p.get(k)) {
                Some(old) if **old == *text => (k, old.clone()),
                _ => {
                    changed = true;
                    (k, Arc::from(text))
                }
            })
            .collect();
        *self.computed_at.lock().unwrap() = Some(Instant::now());

        match prev {
            Some(prev) if !changed => prev,
            _ => {
                let snapshot = Arc::new(Snapshot {
                    sections,
                    full: OnceLock::new(),
                });
                self.tx.send_replace(Some(snapshot.clone()));
                snapshot
            }
        }
    }

    /// Para uma conexão SSE nova: o receptor do `watch` e o snapshot a mandar primeiro (o
    /// publicado, se for recente; senão um calculado na hora). Liga o laço na primeira vez.
    pub(crate) async fn subscribe(
        self: &Arc<Self>,
        state: &AppState,
    ) -> (watch::Receiver<Option<Arc<Snapshot>>>, Arc<Snapshot>) {
        self.ensure_producer(state);
        let mut rx = self.tx.subscribe();
        let recent = self
            .computed_at
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() < TICK);
        let current = rx.borrow_and_update().clone();
        let first = match current {
            Some(snapshot) if recent => snapshot,
            _ => {
                let snapshot = self.refresh(state).await;
                rx.borrow_and_update();
                snapshot
            }
        };
        (rx, first)
    }

    fn ensure_producer(self: &Arc<Self>, state: &AppState) {
        if self.producer_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let hub = self.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Ninguém ouvindo → não calcula nada (antes, sem cliente não havia laço; com
                // cliente havia um laço POR cliente).
                if hub.tx.receiver_count() == 0 {
                    continue;
                }
                hub.refresh(&state).await;
            }
        });
    }
}

/// Serializa `value` direto para texto — sem passar por uma árvore `serde_json::Value`, que para
/// listas grandes (imagens, containers) custava várias vezes o tamanho do próprio JSON.
fn put(out: &mut Vec<(&'static str, String)>, key: &'static str, value: &impl Serialize) {
    out.push((
        key,
        serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
    ));
}

/// As seções do snapshot, cada uma já em JSON, reusando `dispatch` para cada pedaço — o mesmo
/// pacote que o antigo poll de 2 s do cliente buscava. O cliente (Luau/JS) remodela e filtra.
/// `services` abre um `ServiceList` por projeto e marca cada serviço com o nome do projeto.
async fn build_sections(state: &AppState) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::with_capacity(20);

    if let RpResponse::DaemonStatus(d) = dispatch(state.clone(), Command::DaemonStatus).await {
        put(&mut out, "status", &d);
    }
    if let RpResponse::DeploymentSummaries(list) =
        dispatch(state.clone(), Command::RecentDeployments { limit: 40 }).await
    {
        put(&mut out, "deployments", &list);
    }
    if let RpResponse::Projects(projects) = dispatch(state.clone(), Command::ProjectList).await {
        // Uma única listagem do Docker indexa todos os containers por service_id
        // e por projeto Compose, para anexar a cada serviço os containers reais
        // em execução (id + nome + estado) — cobre réplicas, staging e Compose.
        let container_index =
            crate::engine::containers::index_containers(&state.docker.inner).await;
        let mut services = Vec::new();
        let mut projects_json = Vec::new();
        for p in &projects {
            // Só os NOMES dos secrets do projeto (o blob cifrado nunca sai do
            // daemon). Anexados ao objeto do projeto como `containers` é
            // anexado ao do serviço logo abaixo — é dado só do snapshot, então
            // não vira campo do model `Project`.
            let mut pv = serde_json::to_value(p).unwrap_or(Value::Null);
            if let RpResponse::SecretNames(names) = dispatch(
                state.clone(),
                Command::SecretList {
                    project_id: p.id.clone(),
                    service_id: None,
                    job_id: None,
                },
            )
            .await
                && let Value::Object(m) = &mut pv
            {
                m.insert(
                    "secrets".into(),
                    serde_json::to_value(names).unwrap_or(Value::Null),
                );
            }
            projects_json.push(pv);

            if let RpResponse::Services(svcs) = dispatch(
                state.clone(),
                Command::ServiceList {
                    project_id: p.id.clone(),
                },
            )
            .await
            {
                for s in svcs {
                    let conts = container_index.for_service(&s);
                    let mut sv = serde_json::to_value(&s).unwrap_or(Value::Null);
                    if let Value::Object(m) = &mut sv {
                        m.insert(
                            "containers".into(),
                            serde_json::to_value(conts).unwrap_or(Value::Null),
                        );
                    }
                    services.push(json!({ "project_name": p.name, "service": sv }));
                }
            }
        }
        put(&mut out, "projects", &projects_json);
        put(&mut out, "services", &services);
    }
    // Última consulta de versão nova (sem ir ao GitHub): acende o selo nas UIs abertas
    // quando a consulta periódica acha uma release. `null` enquanto ninguém consultou.
    put(
        &mut out,
        "update",
        &state.update_checker.cached_info().await,
    );
    // Painel "Em execução": projetos e serviços rodando, já classificados.
    put(&mut out, "dashboard", &super::dashboard::load(state).await);
    // Selo "aquecido/frio" de cada serviço com build (Git/zip); as telas só exibem.
    if let RpResponse::ServiceWarmStatuses(list) =
        dispatch(state.clone(), Command::ServiceWarmStatusList).await
    {
        put(&mut out, "service_warm", &list);
    }
    if let RpResponse::DockerImages(list) = dispatch(state.clone(), Command::DockerImages).await {
        put(&mut out, "docker_images", &list);
    }
    if let RpResponse::DockerVolumes(list) = dispatch(state.clone(), Command::DockerVolumes).await {
        put(&mut out, "docker_volumes", &list);
    }
    if let RpResponse::DockerNetworks(list) = dispatch(state.clone(), Command::DockerNetworks).await
    {
        put(&mut out, "docker_networks", &list);
    }
    if let RpResponse::DockerContainers(list) =
        dispatch(state.clone(), Command::DockerContainers).await
    {
        put(&mut out, "docker_containers", &list);
    }
    if let RpResponse::JobSummaries(list) = dispatch(state.clone(), Command::JobListAll).await {
        put(&mut out, "jobs", &list);
    }
    if let RpResponse::DeployEngineStatus(eng) =
        dispatch(state.clone(), Command::DeployEngineStatus).await
    {
        put(&mut out, "engine", &eng);
    }
    if let RpResponse::RegistryStatus(info) = dispatch(state.clone(), Command::RegistryStatus).await
    {
        put(&mut out, "registry_status", &info);
    }
    if let RpResponse::RegistryRepos(list) =
        dispatch(state.clone(), Command::RegistryRepoList).await
    {
        put(&mut out, "registry_repos", &list);
    }
    // O que está em claro (ou parece) no estado: a trava da sincronização com o git. Vem no snapshot
    // (e não num RPC à parte) para os dois clientes mostrarem a lista sem polling próprio.
    if let RpResponse::InfraAudit(list) = dispatch(state.clone(), Command::InfraAudit).await {
        put(&mut out, "infra_audit", &list);
    }
    // Situação da sincronização do estado com o git: alimenta o aviso do topo, os selos "mudança
    // pendente de deploy" e a tela Settings → Sincronização, nos dois clientes, sem polling próprio.
    if let RpResponse::StateGit(status) = dispatch(state.clone(), Command::StateGitStatus).await {
        put(&mut out, "state_git", &status);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(sections: Vec<(&'static str, Arc<str>)>) -> Snapshot {
        Snapshot {
            sections,
            full: OnceLock::new(),
        }
    }

    #[test]
    fn full_json_tem_kind_e_todas_as_secoes() {
        let s = snap(vec![
            ("status", Arc::from(r#"{"v":1}"#)),
            ("jobs", Arc::from("[]")),
        ]);
        let v: Value = serde_json::from_str(s.full_json()).unwrap();
        assert_eq!(v["kind"], "snapshot");
        assert_eq!(v["status"]["v"], 1);
        assert_eq!(v["jobs"], json!([]));
    }

    #[test]
    fn patch_so_com_o_que_mudou_e_o_que_sumiu() {
        let igual: Arc<str> = Arc::from("[1]");
        let antes = snap(vec![
            ("status", Arc::from(r#"{"up":1}"#)),
            ("jobs", igual.clone()),
            ("state_git", Arc::from("{}")),
        ]);
        let depois = snap(vec![("status", Arc::from(r#"{"up":2}"#)), ("jobs", igual)]);
        let v: Value = serde_json::from_str(&depois.patch_json(&antes).unwrap()).unwrap();
        assert_eq!(v["kind"], "snapshot_patch");
        assert_eq!(v["status"]["up"], 2);
        assert!(v.get("jobs").is_none(), "seção igual não vai no patch");
        assert_eq!(v["removed"], json!(["state_git"]));
    }

    #[test]
    fn patch_vazio_quando_nada_mudou() {
        let a: Arc<str> = Arc::from("[]");
        let antes = snap(vec![("jobs", a.clone())]);
        let depois = snap(vec![("jobs", a)]);
        assert!(depois.patch_json(&antes).is_none());
    }
}
