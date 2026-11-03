//! Orquestrador de **subida em ordem**: religa serviços respeitando as dependências.
//!
//! Dois usos, o mesmo motor ([`orchestrate`]):
//! - **boot do daemon** ([`spawn_boot`]): sobe os serviços com `desired_state = Running` e
//!   `autostart = true` — o que estava no ar antes do reboot volta; o que o usuário parou à mão
//!   continua parado;
//! - **"Subir projeto/tudo"** (Fase 8): sobe os de `autostart = true` ignorando o `desired_state`.
//!
//! Regras (plano `docs/plano-backup-estado-em-git.md`, R8):
//! - ondas: quem não depende de ninguém sobe primeiro, **em paralelo**; a onda seguinte só começa
//!   quando a anterior terminou;
//! - "pronto" = healthcheck do serviço passou (sem healthcheck: a porta responde por TCP; sem porta:
//!   container rodando);
//! - dependência que falha **bloqueia** os dependentes, com o motivo; os independentes seguem;
//! - **nova tentativa automática** com espera crescente: cada rodada reavalia tudo o que ainda não
//!   ficou pronto, e um dependente sobe assim que a dependência fica pronta (na mesma rodada);
//! - dependências implícitas: todo serviço de um projeto com acesso a um servidor de banco
//!   compartilhado (`shared_access`) espera esse servidor.

use crate::features::deploy::{DeployContext, deps};
use async_trait::async_trait;
use shared::{Event, Service, ServiceSource, ServiceStatus};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tracing::{info, warn};

/// Tempos do orquestrador. Os valores padrão são propostos no plano (R8) e ajustáveis.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Espera antes de cada nova rodada. O tamanho da lista é o número máximo de rodadas extras.
    pub retry_delays: Vec<Duration>,
    /// Quanto esperar um container já existente ficar "pronto".
    pub ready_timeout: Duration,
    /// Quanto esperar um deploy completo (build incluído) terminar.
    pub deploy_timeout: Duration,
    /// Intervalo entre consultas de "pronto".
    pub poll: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            retry_delays: [5, 15, 60, 180, 300]
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            ready_timeout: Duration::from_secs(120),
            deploy_timeout: Duration::from_secs(30 * 60),
            poll: Duration::from_secs(2),
        }
    }
}

/// O que acontece com um serviço durante a subida (para a UI e os logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Starting,
    Ready,
    Failed(String),
    Blocked(String),
}

/// Como subir **um** serviço de verdade. Separado do algoritmo para testá-lo sem Docker.
// O clippy 1.99 (o do CI) acusa o `#[must_use]` que o `async_trait` põe no método com corpo padrão.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Bringer: Send + Sync {
    /// Sobe o serviço e só retorna quando ele está pronto. `Err(motivo)` = não ficou pronto.
    async fn bring_up(&self, svc: &Service) -> Result<(), String>;
    /// Notificação de progresso (status na UI, evento). Padrão: nada.
    async fn report(&self, _svc: &Service, _progress: Progress) {}
}

/// Resultado da última rodada.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub ready: BTreeSet<String>,
    pub failed: BTreeMap<String, String>,
    pub blocked: BTreeMap<String, String>,
}

impl Outcome {
    pub fn all_ready(&self) -> bool {
        self.failed.is_empty() && self.blocked.is_empty()
    }
}

/// Sobe `services` na ordem das dependências. `implicit` são arestas extras `(dependente,
/// dependência)` (ex.: as derivadas de `shared_access`). Dependência fora de `services` é ignorada
/// (ou está parada de propósito, ou é de outra rodada).
pub async fn orchestrate(
    services: Vec<Service>,
    implicit: &[(String, String)],
    bringer: &dyn Bringer,
    policy: &Policy,
) -> Outcome {
    let by_id: BTreeMap<String, Service> =
        services.into_iter().map(|s| (s.id.clone(), s)).collect();
    let mut graph: deps::Graph = by_id
        .iter()
        .map(|(id, s)| (id.clone(), s.spec.depends_on.iter().cloned().collect()))
        .collect();
    for (dependent, dependency) in implicit {
        if dependent != dependency
            && by_id.contains_key(dependent)
            && by_id.contains_key(dependency)
        {
            graph
                .entry(dependent.clone())
                .or_default()
                .insert(dependency.clone());
        }
    }

    let mut outcome = Outcome::default();
    let mut attempt = 0usize;
    loop {
        // Só o que ainda não ficou pronto entra na rodada; o resto já cumpriu sua parte.
        let todo: deps::Graph = graph
            .iter()
            .filter(|(id, _)| !outcome.ready.contains(*id))
            .map(|(id, d)| {
                (
                    id.clone(),
                    d.iter()
                        .filter(|x| !outcome.ready.contains(*x))
                        .cloned()
                        .collect(),
                )
            })
            .collect();
        outcome.failed.clear();
        outcome.blocked.clear();

        let waves = match deps::waves(&todo) {
            Ok(w) => w,
            Err(cycle) => {
                // Não deveria acontecer (a escrita recusa ciclo), mas nunca arrisca uma ordem arbitrária.
                for id in cycle {
                    let why = "dependências em ciclo — nada foi iniciado".to_string();
                    if let Some(svc) = by_id.get(&id) {
                        bringer.report(svc, Progress::Failed(why.clone())).await;
                    }
                    outcome.failed.insert(id, why);
                }
                return outcome;
            }
        };

        for wave in waves {
            let by_id_ref = &by_id;
            let tasks = wave.iter().map(|id| {
                let svc = &by_id_ref[id];
                let blocker = todo[id]
                    .iter()
                    .find(|d| outcome.failed.contains_key(*d) || outcome.blocked.contains_key(*d))
                    .cloned();
                async move {
                    if let Some(dep_id) = blocker {
                        let dep_name = by_id_ref
                            .get(&dep_id)
                            .map(|d| d.spec.name.clone())
                            .unwrap_or(dep_id);
                        let why = format!("dependência \"{dep_name}\" não ficou pronta");
                        bringer.report(svc, Progress::Blocked(why.clone())).await;
                        return (svc.id.clone(), Err((true, why)));
                    }
                    bringer.report(svc, Progress::Starting).await;
                    match bringer.bring_up(svc).await {
                        Ok(()) => {
                            bringer.report(svc, Progress::Ready).await;
                            (svc.id.clone(), Ok(()))
                        }
                        Err(why) => {
                            bringer.report(svc, Progress::Failed(why.clone())).await;
                            (svc.id.clone(), Err((false, why)))
                        }
                    }
                }
            });
            for (id, res) in futures::future::join_all(tasks).await {
                match res {
                    Ok(()) => {
                        outcome.ready.insert(id);
                    }
                    Err((true, why)) => {
                        outcome.blocked.insert(id, why);
                    }
                    Err((false, why)) => {
                        outcome.failed.insert(id, why);
                    }
                }
            }
        }

        if outcome.all_ready() || attempt >= policy.retry_delays.len() {
            return outcome;
        }
        info!(
            falharam = outcome.failed.len(),
            bloqueados = outcome.blocked.len(),
            espera_s = policy.retry_delays[attempt].as_secs(),
            "autostart: nova tentativa agendada"
        );
        tokio::time::sleep(policy.retry_delays[attempt]).await;
        attempt += 1;
    }
}

/// Arestas `(dependente, dependência)` derivadas de `shared_access`: todo serviço de um projeto
/// autorizado espera o servidor de banco compartilhado.
pub fn implicit_edges(
    access: &[(String, String)],
    all_services: &[Service],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (server_id, project_id) in access {
        for svc in all_services {
            if &svc.spec.project_id == project_id && &svc.id != server_id {
                out.push((svc.id.clone(), server_id.clone()));
            }
        }
    }
    out
}

// ── Implementação real (Docker + fila de deploys) ─────────────────────────────

pub struct DockerBringer {
    pub ctx: DeployContext,
    pub policy: Policy,
}

#[async_trait]
impl Bringer for DockerBringer {
    async fn bring_up(&self, svc: &Service) -> Result<(), String> {
        let is_compose = matches!(svc.spec.source, ServiceSource::Compose(_));
        let live = if is_compose {
            Vec::new()
        } else {
            crate::engine::containers::find_live_replicas(&self.ctx.docker.inner, &svc.id)
                .await
                .map_err(|e| format!("não consegui consultar o Docker: {e}"))?
        };

        // Sem container (servidor novo, removido) ou stack Compose: deploy completo, pela fila.
        if live.is_empty() {
            return self.deploy_and_wait(svc).await;
        }

        // Container existe: só liga os parados e espera ficar pronto.
        for replica in live.iter().filter(|r| !r.running) {
            crate::engine::containers::start(&self.ctx.docker.inner, &replica.id)
                .await
                .map_err(|e| format!("docker start falhou: {e}"))?;
        }
        let first = live[0].id.clone();
        self.wait_ready(svc, &first).await?;
        let _ = crate::store::services::update_status(
            &self.ctx.db,
            &svc.id,
            &ServiceStatus::Running,
            Some(&first),
        )
        .await;
        self.ctx.bus.publish(Event::ServiceStatusChanged {
            service_id: svc.id.clone(),
            status: ServiceStatus::Running,
        });
        Ok(())
    }

    async fn report(&self, svc: &Service, progress: Progress) {
        // Só falha/bloqueio vira status visível. "Starting"/"Ready" já têm os seus (o deploy marca
        // Queued/Deploying/Running; o start direto marca Running acima).
        let status = match progress {
            Progress::Failed(why) => ServiceStatus::Error(format!("não subiu no boot: {why}")),
            Progress::Blocked(why) => ServiceStatus::Error(why),
            Progress::Starting | Progress::Ready => return,
        };
        let _ = crate::store::services::update_status(
            &self.ctx.db,
            &svc.id,
            &status,
            svc.live_container_id.as_deref(),
        )
        .await;
        self.ctx.bus.publish(Event::ServiceStatusChanged {
            service_id: svc.id.clone(),
            status,
        });
    }
}

impl DockerBringer {
    async fn deploy_and_wait(&self, svc: &Service) -> Result<(), String> {
        let resp = crate::features::deploy::start::handle(self.ctx.clone(), svc.id.clone()).await;
        let dep = match resp {
            shared::Response::Deployment(d) => d,
            shared::Response::Err { code, message } => return Err(format!("{code}: {message}")),
            outra => {
                return Err(format!(
                    "resposta inesperada ao iniciar o deploy: {outra:?}"
                ));
            }
        };
        let deadline = tokio::time::Instant::now() + self.policy.deploy_timeout;
        loop {
            match crate::store::deployments::get(&self.ctx.db, &dep.id).await {
                Ok(Some(d)) if d.state.is_terminal() => {
                    return if d.state == shared::DeployState::Live {
                        Ok(())
                    } else {
                        Err(format!("o deploy terminou em {}", d.state.label()))
                    };
                }
                Ok(_) => {}
                Err(e) => return Err(format!("erro lendo o deploy: {e}")),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("o deploy não terminou a tempo".into());
            }
            tokio::time::sleep(self.policy.poll).await;
        }
    }

    /// "Pronto": healthcheck do serviço; sem healthcheck, a porta responde (TCP); sem porta,
    /// container rodando.
    async fn wait_ready(&self, svc: &Service, container_id: &str) -> Result<(), String> {
        use shared::HealthcheckKind;
        let hc = &svc.spec.healthcheck;
        let timeout = Duration::from_secs(u64::from(hc.timeout_secs.max(1)));
        let deadline = tokio::time::Instant::now() + self.policy.ready_timeout;
        loop {
            let ok = match &hc.kind {
                HealthcheckKind::None if svc.spec.port == 0 => {
                    crate::features::observability::watchdog::container_is_running(
                        &self.ctx.docker.inner,
                        container_id,
                    )
                    .await
                }
                HealthcheckKind::None => {
                    let tcp = shared::Healthcheck {
                        kind: HealthcheckKind::Tcp,
                        ..hc.clone()
                    };
                    crate::features::observability::watchdog::run_healthcheck(
                        &tcp,
                        container_id,
                        &self.ctx.docker.inner,
                        svc.spec.port,
                        timeout,
                    )
                    .await
                }
                _ => {
                    crate::features::observability::watchdog::run_healthcheck(
                        hc,
                        container_id,
                        &self.ctx.docker.inner,
                        svc.spec.port,
                        timeout,
                    )
                    .await
                }
            };
            if ok {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "não ficou pronto em {}s",
                    self.policy.ready_timeout.as_secs()
                ));
            }
            tokio::time::sleep(self.policy.poll).await;
        }
    }
}

/// Serviços que o boot deve religar: `desired_state = Running` e `autostart = true`.
pub async fn boot_candidates(db: &crate::store::Db) -> anyhow::Result<Vec<Service>> {
    Ok(crate::store::services::get_desired_running(db)
        .await?
        .into_iter()
        .filter(|s| s.spec.autostart)
        .collect())
}

/// Dispara o religamento do boot numa task (não bloqueia a subida do daemon). Chamar **depois**
/// de o worker da fila de deploys estar de pé, pois serviços sem container passam por ela.
pub fn spawn_boot(ctx: DeployContext) {
    tokio::spawn(async move {
        let candidates = match boot_candidates(&ctx.db).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "autostart: não consegui listar os serviços a religar");
                return;
            }
        };
        if candidates.is_empty() {
            return;
        }
        let all = crate::store::services::list_all(&ctx.db)
            .await
            .unwrap_or_default();
        let access = crate::store::shared_access::all(&ctx.db)
            .await
            .unwrap_or_default();
        let implicit = implicit_edges(&access, &all);
        info!(
            servicos = candidates.len(),
            "autostart: religando serviços na ordem das dependências"
        );
        let policy = Policy::default();
        let bringer = DockerBringer {
            ctx: ctx.clone(),
            policy: policy.clone(),
        };
        let out = orchestrate(candidates, &implicit, &bringer, &policy).await;
        if out.all_ready() {
            info!(prontos = out.ready.len(), "autostart: tudo no ar");
        } else {
            warn!(
                prontos = out.ready.len(),
                falharam = out.failed.len(),
                bloqueados = out.blocked.len(),
                "autostart: terminou com serviços fora do ar (veja o status de cada um)"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use shared::{Healthcheck, ResourceLimits, ServiceSpec};
    use std::{collections::HashMap, sync::Mutex};

    fn svc(id: &str, project: &str, deps: &[&str]) -> Service {
        Service {
            id: id.into(),
            spec: ServiceSpec {
                name: format!("nome-{id}"),
                project_id: project.into(),
                source: ServiceSource::Registry { image: "x".into() },
                port: 80,
                host_port: None,
                domain: None,
                tls_enabled: false,
                env_vars: vec![],
                env_comments: vec![],
                volumes: vec![],
                healthcheck: Healthcheck::default(),
                replicas: 1,
                resources: ResourceLimits::default(),
                run_command: None,
                run_args: vec![],
                db_kind: None,
                domains: vec![],
                pre_deploy_job_id: None,
                pre_deploy_job_ids: vec![],
                shared: None,
                depends_on: deps.iter().map(|d| d.to_string()).collect(),
                autostart: true,
            },
            compose_project: None,
            status: ServiceStatus::Stopped,
            live_container_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn fast() -> Policy {
        Policy {
            retry_delays: vec![Duration::from_millis(1); 3],
            ..Policy::default()
        }
    }

    /// Bringer falso: grava a ordem das chamadas e falha as primeiras `n` vezes de cada id.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        failures_left: Mutex<HashMap<String, usize>>,
        reports: Mutex<Vec<(String, Progress)>>,
        barrier: Option<tokio::sync::Barrier>,
    }

    impl Fake {
        fn failing(id: &str, times: usize) -> Self {
            let f = Fake::default();
            f.failures_left.lock().unwrap().insert(id.into(), times);
            f
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn count(&self, id: &str) -> usize {
            self.calls().iter().filter(|c| *c == id).count()
        }
        fn pos(&self, id: &str) -> usize {
            self.calls().iter().position(|c| c == id).unwrap()
        }
    }

    #[async_trait]
    impl Bringer for Fake {
        async fn bring_up(&self, svc: &Service) -> Result<(), String> {
            if let Some(b) = &self.barrier {
                b.wait().await;
            }
            self.calls.lock().unwrap().push(svc.id.clone());
            let mut left = self.failures_left.lock().unwrap();
            if let Some(n) = left.get_mut(&svc.id)
                && *n > 0
            {
                *n -= 1;
                return Err("quebrou".into());
            }
            Ok(())
        }
        async fn report(&self, svc: &Service, p: Progress) {
            self.reports.lock().unwrap().push((svc.id.clone(), p));
        }
    }

    #[tokio::test]
    async fn banco_antes_da_api_antes_do_front() {
        let f = Fake::default();
        let out = orchestrate(
            vec![
                svc("front", "p", &["api"]),
                svc("api", "p", &["db"]),
                svc("db", "p", &[]),
            ],
            &[],
            &f,
            &fast(),
        )
        .await;
        assert!(out.all_ready());
        assert!(
            f.pos("db") < f.pos("api") && f.pos("api") < f.pos("front"),
            "{:?}",
            f.calls()
        );
    }

    #[tokio::test]
    async fn independentes_da_mesma_onda_sobem_em_paralelo() {
        // O Barrier(2) só libera quando os DOIS estão dentro de bring_up ao mesmo tempo:
        // se subissem em sequência, o teste travaria (e o timeout o reprova).
        let f = Fake {
            barrier: Some(tokio::sync::Barrier::new(2)),
            ..Fake::default()
        };
        let policy = fast();
        let run = orchestrate(
            vec![svc("a", "p", &[]), svc("b", "p", &[])],
            &[],
            &f,
            &policy,
        );
        let out = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("subiram em sequência (deadlock no Barrier)");
        assert!(out.all_ready());
    }

    #[tokio::test]
    async fn falha_bloqueia_so_os_dependentes_e_os_independentes_seguem() {
        let f = Fake::failing("db", 99);
        let policy = Policy {
            retry_delays: vec![],
            ..fast()
        };
        let out = orchestrate(
            vec![
                svc("db", "p", &[]),
                svc("api", "p", &["db"]),
                svc("solto", "p", &[]),
            ],
            &[],
            &f,
            &policy,
        )
        .await;
        assert!(out.failed.contains_key("db"));
        assert!(out.blocked["api"].contains("nome-db"), "{:?}", out.blocked);
        assert!(out.ready.contains("solto"));
        assert_eq!(f.count("api"), 0, "o bloqueado não pode nem tentar subir");
        assert!(
            f.reports
                .lock()
                .unwrap()
                .iter()
                .any(|(id, p)| id == "api" && matches!(p, Progress::Blocked(_)))
        );
    }

    #[tokio::test]
    async fn dependencia_que_se_recupera_destrava_o_dependente_sozinho() {
        let f = Fake::failing("db", 1); // falha na 1ª rodada, sobe na 2ª
        let out = orchestrate(
            vec![svc("db", "p", &[]), svc("api", "p", &["db"])],
            &[],
            &f,
            &fast(),
        )
        .await;
        assert!(out.all_ready(), "{out:?}");
        assert_eq!(f.count("db"), 2);
        assert_eq!(
            f.count("api"),
            1,
            "o dependente só sobe depois que a dependência ficou pronta"
        );
        assert!(
            f.calls().iter().rposition(|c| c == "db") < f.calls().iter().position(|c| c == "api")
        );
    }

    #[tokio::test]
    async fn esgota_as_tentativas_e_deixa_o_erro_visivel() {
        let f = Fake::failing("db", 99);
        let policy = fast();
        let out = orchestrate(
            vec![svc("db", "p", &[]), svc("api", "p", &["db"])],
            &[],
            &f,
            &policy, // 3 esperas = 1 + 3 rodadas
        )
        .await;
        assert!(!out.all_ready());
        assert_eq!(f.count("db"), 4);
        assert_eq!(f.count("api"), 0);
    }

    #[tokio::test]
    async fn quem_ja_ficou_pronto_nao_sobe_de_novo_nas_rodadas_seguintes() {
        let f = Fake::failing("b", 1);
        let out = orchestrate(
            vec![svc("a", "p", &[]), svc("b", "p", &[])],
            &[],
            &f,
            &fast(),
        )
        .await;
        assert!(out.all_ready());
        assert_eq!(f.count("a"), 1);
        assert_eq!(f.count("b"), 2);
    }

    #[tokio::test]
    async fn dependencia_fora_do_conjunto_nao_bloqueia() {
        // "parado" não é candidato (o usuário o deixou desligado): não pode travar a api.
        let f = Fake::default();
        let out = orchestrate(vec![svc("api", "p", &["parado"])], &[], &f, &fast()).await;
        assert!(out.all_ready());
    }

    #[tokio::test]
    async fn ciclo_nao_inicia_nada() {
        let f = Fake::default();
        let out = orchestrate(
            vec![svc("a", "p", &["b"]), svc("b", "p", &["a"])],
            &[],
            &f,
            &fast(),
        )
        .await;
        assert!(f.calls().is_empty());
        assert_eq!(out.failed.len(), 2);
    }

    #[tokio::test]
    async fn aresta_implicita_de_banco_compartilhado_ordena_entre_projetos() {
        let servidor = svc("srv", "pa", &[]);
        let cliente = svc("cli", "pb", &[]);
        let access = vec![("srv".to_string(), "pb".to_string())];
        let implicit = implicit_edges(&access, &[servidor.clone(), cliente.clone()]);
        assert_eq!(implicit, vec![("cli".to_string(), "srv".to_string())]);

        let f = Fake::default();
        let out = orchestrate(vec![cliente, servidor], &implicit, &f, &fast()).await;
        assert!(out.all_ready());
        assert!(f.pos("srv") < f.pos("cli"), "{:?}", f.calls());
    }

    #[tokio::test]
    async fn servidor_compartilhado_nao_depende_de_si_mesmo() {
        let servidor = svc("srv", "pb", &[]); // o próprio servidor está num projeto autorizado
        let implicit = implicit_edges(&[("srv".into(), "pb".into())], &[servidor]);
        assert!(implicit.is_empty());
    }
}
