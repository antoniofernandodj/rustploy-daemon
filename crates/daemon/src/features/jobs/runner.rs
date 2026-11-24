//! Execução de um `Job` (tarefa one-shot via docker-compose): resolve rede +
//! env vars do serviço gatilho, sobe o stack até `main_service` terminar
//! (`docker::compose::run_once`), grava o resultado em `job_run`/`job_log` e
//! reagenda (`Job::recurrence`, quando houver).

use crate::domain::{Job, JobRun};
use crate::engine::{self, DockerClient, networks};
use crate::features::jobs::JobsContext;
use crate::platform::event_bus::EventBus;
use crate::platform::secrets::SecretsManager;
use crate::protocol::Event;
use crate::store;
use anyhow::Result;
use chrono::Utc;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::domain::QueueItemKind;
use crate::features::deploy::queue::{QueueTaskFn, QueueTaskInfo};

pub struct JobRunner {
    pub db: Arc<store::Db>,
    pub docker: Arc<DockerClient>,
    pub bus: Arc<EventBus>,
    pub secrets: Arc<SecretsManager>,
    pub db_path: PathBuf,
    pub registry_internal_token: Option<Arc<str>>,
}

/// Cria o `job_run` e dispara a execução em background (`tokio::spawn`) —
/// usado tanto pelo `scheduler_loop` (jobs vencidos) quanto por
/// `Command::JobRunNow` (disparo manual). Retorna assim que o `job_run` é
/// criado; o resultado chega depois via `Event::JobRunStateChanged` +
/// `Command::JobRunHistory`/`GetJobLogs`.
pub async fn spawn(state: &JobsContext, job: Job) -> Result<JobRun> {
    let run = store::job_run::create(&state.db, &job.id).await?;
    let runner = Arc::new(JobRunner {
        db: state.db.clone(),
        docker: state.docker.clone(),
        bus: state.bus.clone(),
        secrets: state.secrets.clone(),
        db_path: state.db_path.clone(),
        registry_internal_token: state.registry_internal_token.clone(),
    });
    let run_id = run.id.clone();

    // Sinal de cancelamento (`Command::JobRunCancel` → `JobsContext::
    // active_jobs`): registrado ANTES da task começar a rodar, removido
    // depois de terminar (sucesso, falha ou cancelamento) — mesmo idioma de
    // `active_deploys`, mas com um `watch<bool>` observado dentro de
    // `docker::compose::run_once_up` em vez de abortar a task (abortar a
    // task não mataria o processo `docker compose up` filho). Só o caminho
    // scheduler/`JobRunNow` (aqui) registra — o `run_inner_mirrored` chamado
    // pelo `DeployExecutor` (`PreDeployCheck`) passa `None`, fora de escopo
    // por ora.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    if let Ok(mut map) = state.active_jobs.lock() {
        map.insert(run_id.clone(), cancel_tx);
    }
    let active_jobs = state.active_jobs.clone();

    // Empurra o próximo horário JÁ, não só ao terminar: o job pode esperar a vez
    // atrás de um deploy longo, e sem isso o agendador o veria "vencido" a cada
    // tick e dispararia outra execução por cima.
    runner.reschedule(&job).await;

    // Uma coisa por vez, e o job é **um item da fila de deploys**: aparece em "Na fila" (com
    // posição), em "Executando" e no histórico, e pode ser promovido, reordenado ou tirado da
    // fila como um deploy. Quem o executa é o worker único, com a vaga que ele já tem (ver
    // `deploy::queue`). O gate de pré-deploy não passa por aqui — roda por dentro de um deploy,
    // que já tem a vaga.
    let queue = state.deploy_queue.clone();
    if !queue.is_idle() {
        runner
            .linha(
                &job.id,
                &run_id,
                "==> Aguardando a vez: há um deploy ou outro job em execução (ou a fila está pausada)",
            )
            .await;
    }

    let info = QueueTaskInfo {
        kind: QueueItemKind::Job,
        job_id: job.id.clone(),
    };
    let run_task: QueueTaskFn = {
        let (runner, job, run_id, active_jobs) = (
            runner.clone(),
            job.clone(),
            run_id.clone(),
            active_jobs.clone(),
        );
        Box::new(move || {
            Box::pin(async move {
                runner
                    .linha(&job.id, &run_id, "==> Sua vez: iniciando")
                    .await;
                runner.run(&job, run_id.clone(), Some(cancel_rx)).await;
                if let Ok(mut map) = active_jobs.lock() {
                    map.remove(&run_id);
                }
            })
        })
    };
    // Tirada da fila antes da vez: nunca começou; fecha como cancelada.
    let on_cancel: QueueTaskFn = {
        let (runner, job, run_id) = (runner.clone(), job.clone(), run_id.clone());
        Box::new(move || {
            Box::pin(async move {
                runner.cancelar_antes_de_comecar(&job, &run_id).await;
                if let Ok(mut map) = active_jobs.lock() {
                    map.remove(&run_id);
                }
            })
        })
    };
    queue.enqueue_task(run_id, info, run_task, on_cancel);
    state.bus.publish(Event::DeployQueueChanged);
    Ok(run)
}

impl JobRunner {
    pub async fn run(
        &self,
        job: &Job,
        run_id: String,
        cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
    ) {
        info!(job_id = %job.id, run_id = %run_id, "job_runner: iniciando execução");
        self.bus.publish(Event::JobRunStateChanged {
            job_id: job.id.clone(),
            job_run_id: run_id.clone(),
            running: true,
            success: None,
        });

        let result = self.run_inner_mirrored(job, &run_id, None, cancel_rx).await;
        let success = match &result {
            Ok(exit_code) => *exit_code == 0,
            Err(e) => {
                error!(job_id = %job.id, run_id = %run_id, error = %e, "job_runner: falha ao executar job");
                let _ = store::job_run::finish(&self.db, &run_id, -1).await;
                false
            }
        };

        self.bus.publish(Event::JobRunStateChanged {
            job_id: job.id.clone(),
            job_run_id: run_id.clone(),
            running: false,
            success: Some(success),
        });

        self.reschedule(job).await;
    }

    /// Roda `job` até o `main_service` terminar e devolve o exit code — sem
    /// tocar em `job.recurrence`/`reschedule` (isso é responsabilidade de
    /// `run()`, chamado pelo scheduler/`JobRunNow`). `mirror_deployment`,
    /// quando `Some((deployment_id, service_id))`, também espelha a saída
    /// como `Event::BuildLog` — usado pelo `DeployExecutor` no estado
    /// `PreDeployCheck` (ver `docs/plano-pre-deploy-gate.md`) pra a saída do
    /// check aparecer na tela de deploy; nesse caminho `cancel_rx` é sempre
    /// `None` (cancelamento de job só é suportado no caminho scheduler/
    /// `JobRunNow`, via `spawn()` — ver `docs/plano-cancelamento-de-jobs.md`).
    pub(crate) async fn run_inner_mirrored(
        &self,
        job: &Job,
        run_id: &str,
        mirror_deployment: Option<(String, String)>,
        cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<i32> {
        let project_net = crate::store::projects::network_name(&self.db, &job.project_id).await?;
        let network_name =
            networks::ensure_project_network(&self.docker.inner, &project_net).await?;

        // Base (projeto [+ serviço gatilho]) + overrides do próprio job, maior
        // precedência — ver deploy::env_resolve::resolve_job.
        let env_vars =
            crate::features::deploy::env_resolve::resolve_job(&self.db, &self.secrets, job).await?;

        // Nome de projeto do compose: só minúsculas/dígitos/`_`/`-` (regra do
        // próprio `docker compose`) — o run_id (ULID) tem letras maiúsculas.
        let project_name = run_id.to_lowercase();
        let build_dir = self.db_path.join("jobs").join(run_id);

        // Job git-sourced: clona pra dentro do PRÓPRIO build_dir (vira o
        // checkout) antes do compose — mesma resolução de credenciais do
        // deploy de serviço git-sourced (`deploy::git::resolve_clone_
        // credentials`), reusada aqui pra não duplicar. O progresso do clone
        // vira linha de log do job (mesmo bus_batch que o stdout do compose),
        // pra aparecer ao vivo em "Ver logs" igual o resto da execução.
        let build_source = match &job.git_source {
            Some(git) => {
                let bus = self.bus.clone();
                let db_handle = self.db.clone();
                let jid = job.id.clone();
                let rid = run_id.to_string();
                super::source::clone_git_source(
                    &self.db,
                    &self.secrets,
                    job,
                    git,
                    &build_dir,
                    move |p| {
                        let ts = Utc::now();
                        bus.publish(Event::JobLogLine {
                            job_run_id: rid.clone(),
                            job_id: jid.clone(),
                            line: p.description.clone(),
                            timestamp: ts,
                            stream: crate::protocol::LogStream::Stdout,
                        });
                        let db_handle = db_handle.clone();
                        let rid = rid.clone();
                        let line = p.description;
                        tokio::spawn(async move {
                            let _ = store::job_log::append(
                                &db_handle,
                                &rid,
                                &crate::protocol::LogStream::Stdout,
                                &line,
                                ts,
                            )
                            .await;
                        });
                    },
                )
                .await?;

                engine::compose::JobBuildSource::Git {
                    compose_rel_path: &git.compose_path,
                }
            }
            None => engine::compose::JobBuildSource::Compose(&job.compose),
        };

        // Cancelamento pode chegar durante um clone Git lento (não
        // interrompido no meio — só checado depois); sem isso, o job
        // seguiria pra subir containers mesmo já tendo sido cancelado.
        let already_cancelled = cancel_rx.as_ref().map(|rx| *rx.borrow()).unwrap_or(false);
        let (exit_code, images) = if already_cancelled {
            (engine::compose::CANCELLED_EXIT_CODE, None)
        } else {
            engine::compose::run_once(
                build_source,
                &project_name,
                &network_name,
                &job.main_service,
                &job.id,
                run_id,
                &self.bus,
                &self.db,
                &env_vars,
                &build_dir,
                self.registry_internal_token.clone(),
                mirror_deployment,
                cancel_rx,
            )
            .await?
        };

        store::job_run::finish(&self.db, run_id, exit_code).await?;

        if let Some(images) = images {
            super::warm::record(&self.db, job, &images).await;
        }

        if let Err(e) = tokio::fs::remove_dir_all(&build_dir).await {
            warn!(run_id = %run_id, error = %e, "job_runner: falha ao limpar build_dir (best-effort)");
        }

        Ok(exit_code)
    }

    /// Uma linha no log da execução (banco + evento ao vivo).
    async fn linha(&self, job_id: &str, run_id: &str, texto: &str) {
        let ts = Utc::now();
        self.bus.publish(Event::JobLogLine {
            job_run_id: run_id.to_string(),
            job_id: job_id.to_string(),
            line: texto.to_string(),
            timestamp: ts,
            stream: crate::protocol::LogStream::Stdout,
        });
        let _ = store::job_log::append(
            &self.db,
            run_id,
            &crate::protocol::LogStream::Stdout,
            texto,
            ts,
        )
        .await;
    }

    /// Cancelada enquanto esperava a vez: nunca começou. Fecha a execução como
    /// cancelada (o mesmo exit code do cancelamento durante a execução).
    async fn cancelar_antes_de_comecar(&self, job: &Job, run_id: &str) {
        self.linha(&job.id, run_id, "==> Cancelado antes de começar a executar")
            .await;
        let _ =
            store::job_run::finish(&self.db, run_id, engine::compose::CANCELLED_EXIT_CODE).await;
        self.bus.publish(Event::JobRunStateChanged {
            job_id: job.id.clone(),
            job_run_id: run_id.to_string(),
            running: false,
            success: Some(false),
        });
    }

    async fn reschedule(&self, job: &Job) {
        let now = Utc::now();
        let next_run_at = job.recurrence.map(|r| r.next_after(now));
        if let Err(e) = store::job::mark_fired(&self.db, &job.id, now, next_run_at).await {
            error!(job_id = %job.id, error = %e, "job_runner: falha ao reagendar");
        }
    }
}

/// Fecha o `job_run` de um pré-deploy check se o deploy for **abortado** no meio.
///
/// O gate de pré-deploy (`DeployExecutor::step_pre_deploy_check`) abre o `job_run`
/// e roda o job dentro da task do deploy. `DeployAbort` derruba essa task: sem este
/// guarda, ninguém fechava o `job_run`, que ficava "em execução" até o próximo boot
/// do daemon, e o `JobRunCancel` respondia que ele não existia. Criado armado logo
/// depois do `job_run::create`; `disarm()` quando o job termina por conta própria.
pub(crate) struct PreDeployRunGuard {
    db: Arc<store::Db>,
    bus: Arc<EventBus>,
    job_id: String,
    run_id: String,
    /// `<db_path>/jobs/<run_id>`, removido junto quando o deploy é abortado.
    build_dir: Option<PathBuf>,
    armed: bool,
}

impl PreDeployRunGuard {
    pub(crate) fn new(db: Arc<store::Db>, bus: Arc<EventBus>, job_id: &str, run_id: &str) -> Self {
        Self {
            db,
            bus,
            job_id: job_id.to_string(),
            run_id: run_id.to_string(),
            build_dir: None,
            armed: true,
        }
    }

    /// Diretório de build do job (`<db_path>/jobs/<run_id>`), para limpar no abort.
    pub(crate) fn with_build_dir(mut self, db_path: &std::path::Path) -> Self {
        self.build_dir = Some(db_path.join("jobs").join(&self.run_id));
        self
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PreDeployRunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // Sem runtime (encerrando o processo): o `recover_interrupted` do próximo boot fecha.
            return;
        };
        let (db, bus) = (self.db.clone(), self.bus.clone());
        let (job_id, run_id) = (self.job_id.clone(), self.run_id.clone());
        let build_dir = self.build_dir.take();
        handle.spawn(async move {
            close_aborted_run(&db, &bus, &job_id, &run_id).await;
            clean_aborted_run(&run_id, build_dir).await;
        });
    }
}

/// Fecha como cancelada (mesmo exit code do `JobRunCancel`) uma execução que ainda
/// não terminou, com uma linha no log dizendo por quê. Não toca numa que já terminou.
pub(crate) async fn close_aborted_run(db: &store::Db, bus: &EventBus, job_id: &str, run_id: &str) {
    match store::job_run::get(db, run_id).await {
        Ok(Some(run)) if run.finished_at.is_none() => {}
        _ => return,
    }
    let _ = store::job_log::append(
        db,
        run_id,
        &crate::protocol::LogStream::Stderr,
        "==> Interrompida: o deploy que rodava este pré-deploy check foi abortado",
        Utc::now(),
    )
    .await;
    let _ = store::job_run::finish(db, run_id, engine::compose::CANCELLED_EXIT_CODE).await;
    bus.publish(Event::JobRunStateChanged {
        job_id: job_id.to_string(),
        job_run_id: run_id.to_string(),
        running: false,
        success: Some(false),
    });
}

/// O resto do que o `run_once` faria no fim e a future abortada não fez: os containers
/// do job (o projeto compose é o run_id em minúsculas, ver `run_inner_mirrored`) e o
/// diretório de build.
async fn clean_aborted_run(run_id: &str, build_dir: Option<PathBuf>) {
    engine::compose::remove_project_containers(&run_id.to_lowercase()).await;
    if let Some(dir) = build_dir {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}

#[cfg(test)]
mod pre_deploy_guard_tests {
    use super::*;

    async fn mem_db() -> Arc<store::Db> {
        let dir = std::env::temp_dir().join(format!("rustploy_test_{}", ulid::Ulid::new()));
        Arc::new(store::connect(&dir).await.unwrap())
    }

    #[tokio::test]
    async fn guarda_armado_fecha_a_execucao_como_cancelada_ao_ser_derrubado() {
        let db = mem_db().await;
        let bus = Arc::new(EventBus::new());
        let run = store::job_run::create(&db, "job_pre").await.unwrap();

        // A task do deploy é abortada: a future (e o guarda dentro dela) é derrubada.
        let task = {
            let (db, bus, id) = (db.clone(), bus.clone(), run.id.clone());
            tokio::spawn(async move {
                let _guard = PreDeployRunGuard::new(db, bus, "job_pre", &id);
                std::future::pending::<()>().await;
            })
        };
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;

        // O fechamento roda numa task própria; dá a vez a ela.
        let mut fechada = None;
        for _ in 0..50 {
            let r = store::job_run::get(&db, &run.id).await.unwrap().unwrap();
            if r.finished_at.is_some() {
                fechada = Some(r);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let r = fechada.expect("o job_run deveria ter sido fechado");
        assert_eq!(r.exit_code, Some(engine::compose::CANCELLED_EXIT_CODE));
        assert_eq!(r.success, Some(false));
        let log = store::job_log::get_for_run(&db, &run.id).await.unwrap();
        assert!(log.iter().any(|l| l.line.contains("abortado")), "{log:?}");
    }

    #[tokio::test]
    async fn guarda_desarmado_nao_mexe_na_execucao() {
        let db = mem_db().await;
        let bus = Arc::new(EventBus::new());
        let run = store::job_run::create(&db, "job_pre").await.unwrap();
        store::job_run::finish(&db, &run.id, 0).await.unwrap();
        {
            let mut guard = PreDeployRunGuard::new(db.clone(), bus.clone(), "job_pre", &run.id);
            guard.disarm();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let r = store::job_run::get(&db, &run.id).await.unwrap().unwrap();
        assert_eq!(r.exit_code, Some(0), "terminou com sucesso e assim fica");
        assert!(
            store::job_log::get_for_run(&db, &run.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn fechamento_nao_sobrescreve_execucao_ja_terminada() {
        let db = mem_db().await;
        let bus = EventBus::new();
        let run = store::job_run::create(&db, "job_pre").await.unwrap();
        store::job_run::finish(&db, &run.id, 0).await.unwrap();
        close_aborted_run(&db, &bus, "job_pre", &run.id).await;
        let r = store::job_run::get(&db, &run.id).await.unwrap().unwrap();
        assert_eq!(r.exit_code, Some(0));
    }
}
