//! Execução de jobs one-shot via compose: arquivos, imagens, aquecimento e cancelamento.

use crate::platform::{event_bus::EventBus, log_sink::LogSink};
use anyhow::{Result, anyhow};
use bollard::Docker;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::fs::{File, remove_file};
use tokio::io::AsyncWriteExt;
use tracing::info;

use super::*;

/// Sobe um stack docker-compose UMA VEZ (job one-shot, não um serviço de vida
/// longa): usa `--abort-on-container-exit --exit-code-from <main_service>` —
/// a própria flag do `docker compose` feita pra isso — em vez de `-d`, então
/// o processo espera `main_service` terminar e propaga o exit code dele.
/// Sempre roda `down(...)` depois (sucesso ou falha), pra não deixar
/// containers/redes zumbis do job. Retorna o exit code de `main_service`.
///
/// `mirror_deployment`, quando `Some((deployment_id, service_id))`, também
/// espelha cada linha como `Event::BuildLog`/`build_log` do deployment —
/// usado pelo `DeployExecutor` no estado `PreDeployCheck` pra a saída do
/// job aparecer na tela de deploy, além do histórico normal do Job.
/// De onde vem o `docker-compose.yml` de um `run_once`. `Compose` é o modo
/// original (cola o YAML na mão); `Git` é usado quando `Job.git_source` é
/// `Some` — o repo já foi clonado pra dentro de `build_dir` (ver
/// `jobs::runner`) antes de chegar aqui, então só falta ler o arquivo.
pub enum JobBuildSource<'a> {
    Compose(&'a str),
    /// Caminho relativo a `build_dir` de um `docker-compose.yml` já presente
    /// no clone (ex.: `"docker-compose.yml"` ou `"ci/docker-compose.yml"`).
    Git {
        compose_rel_path: &'a str,
    },
}

/// Exit code sentinel pra "job_run cancelado pelo usuário" (`JobRunCancel`)
/// — distinto do `-1` genérico (processo morto por sinal por outro motivo),
/// pra a UI poder rotular "Cancelado" em vez de "Falhou". Não exige mudança
/// de schema: `job_run.exit_code` já é um inteiro livre. Ver
/// `docs/plano-cancelamento-de-jobs.md`.
pub const CANCELLED_EXIT_CODE: i32 = -2;

/// Espera até o sinal de cancelamento virar `true` (ou o sender ser
/// derrubado, o que só acontece se a task inteira já tiver sumido — nesse
/// caso não há mais nada a cancelar). `watch::Receiver` rastreia versão, não
/// só o valor: mesmo que `true` já tenha sido enviado ANTES desta chamada
/// começar a observar, `changed()` retorna na hora — sem race de "perder" um
/// cancelamento que chegou cedo demais.
pub(super) async fn wait_for_cancel(rx: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Arquivos de um job escritos em `build_dir`: o compose (com a rede do projeto
/// injetada, no mesmo lugar do original) e o `.env`.
pub(super) struct JobFiles {
    compose_rel_path: String,
    /// Compose já com a rede do projeto — o que o `down` recebe.
    content: String,
    env_file_path: std::path::PathBuf,
    compose_file_path: std::path::PathBuf,
}

impl JobFiles {
    async fn remove(&self) {
        let _ = remove_file(&self.env_file_path).await;
        let _ = remove_file(&self.compose_file_path).await;
    }
}

/// Prepara o compose (colado ou do repositório git) com a rede do projeto e
/// grava o `.env`. O compose é reescrito no MESMO lugar após
/// `inject_project_network` — nunca movido, pra `build:` relativo no compose do
/// usuário continuar resolvendo certo (o Compose resolve contexto de build
/// relativo à posição do arquivo `-f`, não do cwd).
pub(super) async fn write_job_files(
    source: JobBuildSource<'_>,
    network_name: &str,
    env_vars: &[(String, String)],
    build_dir: &Path,
) -> Result<JobFiles> {
    tokio::fs::create_dir_all(build_dir).await?;

    let (raw_content, compose_rel_path): (String, String) = match source {
        JobBuildSource::Compose(content) => (content.to_string(), "docker-compose.yml".to_string()),
        JobBuildSource::Git { compose_rel_path } => {
            let path = build_dir.join(compose_rel_path);
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| anyhow!("compose não encontrado em {}: {e}", path.display()))?;
            (content, compose_rel_path.to_string())
        }
    };
    let content = inject_project_network(&raw_content, network_name)?;

    let env_file_path = build_dir.join(".env");
    let compose_file_path = build_dir.join(&compose_rel_path);

    {
        let mut env_file = File::create(&env_file_path).await?;
        for (k, v) in env_vars {
            env_file
                .write_all(format!("{}={}\n", k, v).as_bytes())
                .await?;
        }
        env_file.flush().await?;
    }

    let mut compose_file = File::create(&compose_file_path).await?;
    compose_file.write_all(content.as_bytes()).await?;
    compose_file.flush().await?;

    Ok(JobFiles {
        compose_rel_path,
        content,
        env_file_path,
        compose_file_path,
    })
}

/// O que o compose de um job precisa ter no host para rodar sem baixar nem
/// compilar do zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComposeImages {
    /// Imagens de serviços sem `build:`, que o compose baixa de um registry.
    pub pulled_images: Vec<String>,
    /// Algum serviço tem `build:` — depende do cache de build.
    pub has_build: bool,
}

/// Extrai `ComposeImages` da saída de `docker compose config --format json`
/// (já com variáveis interpoladas).
pub(super) fn parse_compose_images(config_json: &str) -> Result<ComposeImages> {
    let config: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|e| anyhow!("saída de docker compose config inválida: {e}"))?;
    let mut images = ComposeImages::default();
    if let Some(services) = config.get("services").and_then(|v| v.as_object()) {
        for service in services.values() {
            if service.get("build").is_some() {
                images.has_build = true;
            } else if let Some(image) = service.get("image").and_then(|v| v.as_str()) {
                images.pulled_images.push(image.to_string());
            }
        }
    }
    images.pulled_images.sort();
    images.pulled_images.dedup();
    Ok(images)
}

pub(super) async fn resolve_compose_images(
    project_name: &str,
    compose_rel_path: &str,
    build_dir: &Path,
) -> Result<ComposeImages> {
    let output = crate::engine::cli()
        .args([
            "compose",
            "-p",
            project_name,
            "-f",
            compose_rel_path,
            "--env-file",
            ".env",
            "config",
            "--format",
            "json",
        ])
        .current_dir(build_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| anyhow!("falha ao iniciar docker compose config: {e}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "docker compose config falhou: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_compose_images(&String::from_utf8_lossy(&output.stdout))
}

/// Aquece um job sem executá-lo: baixa as imagens de registry que faltam
/// (`docker pull`, só das ausentes) e roda `docker compose build` para popular o
/// cache de build. Devolve o que o job precisa ter no host, para registrar o
/// aquecimento. Os filhos morrem se a tarefa for abortada (`kill_on_drop`).
///
/// `force` rebaixa TODAS as imagens de registry (não só as ausentes) e roda o
/// build sem cache e com `--pull` (imagem base nova): é o "reconstruir do zero".
#[allow(clippy::too_many_arguments)]
pub async fn warm(
    source: JobBuildSource<'_>,
    project_name: &str,
    network_name: &str,
    env_vars: &[(String, String)],
    build_dir: &Path,
    registry_internal_token: Option<Arc<str>>,
    docker: &Docker,
    force: bool,
) -> Result<ComposeImages> {
    info!(project = %project_name, force, "compose_warm: iniciando aquecimento");
    let files = write_job_files(source, network_name, env_vars, build_dir).await?;

    if let Some(token) = &registry_internal_token
        && let Err(e) = registry_login(token).await
    {
        tracing::warn!(error = %e, project = %project_name, "compose_warm: falha ao autenticar no registry embutido, pull vai falhar se a imagem for de lá");
    }

    let result = warm_images(
        project_name,
        &files.compose_rel_path,
        build_dir,
        docker,
        force,
    )
    .await;

    if registry_internal_token.is_some()
        && let Err(e) = registry_logout().await
    {
        tracing::warn!(error = %e, project = %project_name, "compose_warm: falha ao deslogar do registry embutido (best-effort)");
    }
    files.remove().await;
    result
}

pub(super) async fn warm_images(
    project_name: &str,
    compose_rel_path: &str,
    build_dir: &Path,
    docker: &Docker,
    force: bool,
) -> Result<ComposeImages> {
    let images = resolve_compose_images(project_name, compose_rel_path, build_dir).await?;

    for image in &images.pulled_images {
        if !force && docker.inspect_image(image).await.is_ok() {
            continue;
        }
        let output = crate::engine::cli()
            .args(["pull", image])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow!("falha ao iniciar docker pull: {e}"))?;
        if !output.status.success() {
            return Err(anyhow!(
                "docker pull {image} falhou: {}",
                stderr_tail(&output.stderr)
            ));
        }
    }

    if images.has_build {
        let output = crate::engine::cli()
            .args(warm_build_args(project_name, compose_rel_path, force))
            .current_dir(build_dir)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow!("falha ao iniciar docker compose build: {e}"))?;
        if !output.status.success() {
            return Err(anyhow!(
                "docker compose build falhou: {}",
                stderr_tail(&output.stderr)
            ));
        }
    }
    Ok(images)
}

/// Argumentos do `compose build` do aquecimento. `force` acrescenta
/// `--no-cache --pull`: refaz todas as camadas e rebaixa as imagens base.
fn warm_build_args(project_name: &str, compose_rel_path: &str, force: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "compose",
        "-p",
        project_name,
        "-f",
        compose_rel_path,
        "--env-file",
        ".env",
        "build",
    ]
    .map(String::from)
    .to_vec();
    if force {
        args.push("--no-cache".to_string());
        args.push("--pull".to_string());
    }
    args
}

/// Executa um job uma vez: prepara o compose (colado ou do repositório git) com
/// a rede do projeto, roda `run_once_up` e sempre desmonta a stack no fim,
/// mesmo se falhou. Além do exit code, devolve as imagens do compose quando o
/// job terminou com sucesso (a execução baixou e compilou tudo, ou seja, o job
/// ficou aquecido).
#[allow(clippy::too_many_arguments)]
pub async fn run_once(
    source: JobBuildSource<'_>,
    project_name: &str,
    network_name: &str,
    main_service: &str,
    job_id: &str,
    job_run_id: &str,
    bus: &Arc<EventBus>,
    db: &Arc<impl LogSink>,
    env_vars: &[(String, String)],
    build_dir: &Path,
    registry_internal_token: Option<Arc<str>>,
    mirror_deployment: Option<(String, String)>,
    cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(i32, Option<ComposeImages>)> {
    info!(project = %project_name, job_id = %job_id, "compose_run_once: iniciando job one-shot");

    let files = write_job_files(source, network_name, env_vars, build_dir).await?;

    if let Some(token) = &registry_internal_token
        && let Err(e) = registry_login(token).await
    {
        tracing::warn!(error = %e, project = %project_name, job_id = %job_id, "compose_run_once: falha ao autenticar no registry embutido, pull vai falhar se a imagem for de lá");
    }

    let up_result = run_once_up(
        project_name,
        &files.compose_rel_path,
        main_service,
        job_id,
        job_run_id,
        bus,
        db,
        build_dir,
        mirror_deployment,
        cancel_rx,
    )
    .await;

    if registry_internal_token.is_some()
        && let Err(e) = registry_logout().await
    {
        tracing::warn!(error = %e, project = %project_name, job_id = %job_id, "compose_run_once: falha ao deslogar do registry embutido (best-effort)");
    }

    let images = if matches!(up_result, Ok(0)) {
        resolve_compose_images(project_name, &files.compose_rel_path, build_dir)
            .await
            .ok()
    } else {
        None
    };

    // Sempre desmonta o stack, mesmo se o up falhou — nunca deixa
    // containers/redes zumbis do job pra trás.
    if let Err(e) = down(&files.content, project_name, network_name, env_vars).await {
        tracing::warn!(project = %project_name, job_id = %job_id, error = %e, "compose_run_once: down falhou (limpeza best-effort)");
    }
    files.remove().await;

    up_result.map(|exit_code| (exit_code, images))
}

/// De quanto em quanto tempo o vigia olha o container do `main_service`.
const EXIT_WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// Quanto tempo o `docker compose up` tem para sair sozinho depois que o
/// `main_service` terminou. Passou disso, o compose travou (visto com Podman:
/// o compose não recebe o evento de saída e espera para sempre).
const EXIT_GRACE: Duration = Duration::from_secs(30);

/// Estado do container do `main_service` como o motor o vê.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum MainServiceState {
    /// Ainda não existe (o compose está compilando ou baixando) ou está rodando.
    NotExited,
    Exited {
        container_id: String,
        exit_code: i32,
    },
}

/// Interpreta a saída de `inspect --format '{{.State.Status}}|{{.State.ExitCode}}'`.
/// `exited`/`dead`/`stopped` (Podman) contam como saída; o resto, não.
pub(super) fn parse_container_state(container_id: &str, inspect_out: &str) -> MainServiceState {
    let (status, code) = inspect_out
        .trim()
        .split_once('|')
        .unwrap_or((inspect_out.trim(), ""));
    let saiu = matches!(status.trim(), "exited" | "dead" | "stopped");
    match (saiu, code.trim().parse::<i32>()) {
        (true, Ok(exit_code)) => MainServiceState::Exited {
            container_id: container_id.to_string(),
            exit_code,
        },
        _ => MainServiceState::NotExited,
    }
}

/// Decide quando o compose travou: o `main_service` saiu e continua saído por
/// `grace` sem o compose sair junto. Volta a zero se o container reaparecer
/// rodando (um `restart:` no compose).
#[derive(Debug, Default)]
pub(super) struct ExitWatch {
    exited_since: Option<Instant>,
}

impl ExitWatch {
    pub(super) fn observe(
        &mut self,
        state: &MainServiceState,
        now: Instant,
        grace: Duration,
    ) -> bool {
        match state {
            MainServiceState::NotExited => {
                self.exited_since = None;
                false
            }
            MainServiceState::Exited { .. } => {
                let since = *self.exited_since.get_or_insert(now);
                now.duration_since(since) >= grace
            }
        }
    }
}

/// Olha o container do `main_service` pelos rótulos que o compose põe (nos dois
/// motores). Qualquer falha do CLI vira `NotExited`: o vigia nunca encerra um job
/// por não conseguir olhar.
async fn main_service_state(project_name: &str, main_service: &str) -> MainServiceState {
    let ps = crate::engine::cli()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=com.docker.compose.project={project_name}"),
            "--filter",
            &format!("label=com.docker.compose.service={main_service}"),
            "--format",
            "{{.ID}}",
        ])
        .kill_on_drop(true)
        .output()
        .await;
    let Ok(ps) = ps else {
        return MainServiceState::NotExited;
    };
    let ids = String::from_utf8_lossy(&ps.stdout);
    let Some(id) = ids.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return MainServiceState::NotExited;
    };
    let inspect = crate::engine::cli()
        .args([
            "inspect",
            "--format",
            "{{.State.Status}}|{{.State.ExitCode}}",
            id,
        ])
        .kill_on_drop(true)
        .output()
        .await;
    match inspect {
        Ok(out) if out.status.success() => {
            parse_container_state(id, &String::from_utf8_lossy(&out.stdout))
        }
        _ => MainServiceState::NotExited,
    }
}

/// Espera até o compose ser dado como travado; devolve o container e o exit code.
async fn wait_for_stuck_compose(project_name: &str, main_service: &str) -> (String, i32) {
    let mut watch = ExitWatch::default();
    loop {
        tokio::time::sleep(EXIT_WATCH_INTERVAL).await;
        let state = main_service_state(project_name, main_service).await;
        if watch.observe(&state, Instant::now(), EXIT_GRACE)
            && let MainServiceState::Exited {
                container_id,
                exit_code,
            } = state
        {
            return (container_id, exit_code);
        }
    }
}

/// Remove (forçado) os containers de um projeto compose pelo rótulo, sem precisar do
/// arquivo. Para o job cuja execução foi abandonada sem passar pelo `down` (deploy
/// abortado no meio do pré-deploy: a future morre antes do `down` do `run_once`).
pub async fn remove_project_containers(project_name: &str) {
    let ps = crate::engine::cli()
        .args([
            "ps",
            "-a",
            "-q",
            "--filter",
            &format!("label=com.docker.compose.project={project_name}"),
        ])
        .kill_on_drop(true)
        .output()
        .await;
    let Ok(ps) = ps else { return };
    let ids: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    if ids.is_empty() {
        return;
    }
    let mut args = vec!["rm".to_string(), "-f".to_string()];
    args.extend(ids);
    let _ = crate::engine::cli()
        .args(&args)
        .kill_on_drop(true)
        .output()
        .await;
}

/// Últimas linhas de saída de um container (stdout e stderr juntos), pelo CLI.
async fn container_log_lines(container_id: &str, tail: usize) -> Vec<String> {
    let out = crate::engine::cli()
        .args(["logs", "--tail", &tail.to_string(), container_id])
        .kill_on_drop(true)
        .output()
        .await;
    let Ok(out) = out else { return Vec::new() };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text.lines().filter_map(clean_compose_line).collect()
}

/// Mata o grupo de processos inteiro do `docker compose up` de um job.
///
/// Com Podman o comando é `podman --remote compose …`, que roda o plugin
/// `docker-compose` como **neto**: matar só o filho (o `start_kill`, o `kill_on_drop`)
/// deixava o plugin órfão e vivo para sempre (visto no cancelamento, no vigia e no
/// deploy abortado). O compose nasce num grupo próprio (`process_group(0)`), e este
/// guarda manda SIGKILL ao grupo quando o compose é abandonado: vigia, cancelamento
/// ou a future derrubada (deploy abortado). `disarm()` quando ele saiu sozinho.
pub(super) struct ComposeGroupGuard {
    pgid: Option<u32>,
}

impl ComposeGroupGuard {
    pub(super) fn new(pgid: Option<u32>) -> Self {
        Self { pgid }
    }

    pub(super) fn kill_now(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_process_group(pgid);
        }
    }

    pub(super) fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for ComposeGroupGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// `kill -KILL -- -<pgid>`. Grupo que já não existe não é erro.
fn kill_process_group(pgid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Roda o `docker compose up` de um job até o `main_service` sair, transmitindo
/// stdout/stderr para o log e matando o processo se chegar o sinal de
/// cancelamento. Um vigia (`wait_for_stuck_compose`) encerra o compose se ele
/// não sair sozinho depois que o `main_service` terminou.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_once_up(
    project_name: &str,
    compose_rel_path: &str,
    main_service: &str,
    job_id: &str,
    job_run_id: &str,
    bus: &Arc<EventBus>,
    db: &Arc<impl LogSink>,
    build_dir: &Path,
    mirror_deployment: Option<(String, String)>,
    cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<i32> {
    let mut command = crate::engine::cli();
    command
        .args([
            "compose",
            "-p",
            project_name,
            "-f",
            compose_rel_path,
            "--env-file",
            ".env",
            "up",
            "--build",
            "--abort-on-container-exit",
            "--exit-code-from",
            main_service,
            "--remove-orphans",
        ])
        .current_dir(build_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Deploy abortado (a future do gate de pré-deploy é derrubada): o compose morre junto.
        .kill_on_drop(true);
    // Grupo próprio, para o `ComposeGroupGuard` matar também o plugin neto do Podman.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| anyhow!("falha ao iniciar docker compose: {e}"))?;
    let mut group = ComposeGroupGuard::new(child.id());

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mirror = mirror_deployment.as_ref();
    let compose_tail = Mutex::new(VecDeque::new());
    let main_lines = AtomicUsize::new(0);
    let read_stdout = read_compose_output(
        stdout,
        crate::protocol::LogStream::Stdout,
        main_service,
        bus,
        db,
        job_id,
        job_run_id,
        mirror,
        &compose_tail,
        &main_lines,
    );
    let read_stderr = read_compose_output(
        stderr,
        crate::protocol::LogStream::Stderr,
        main_service,
        bus,
        db,
        job_id,
        job_run_id,
        mirror,
        &compose_tail,
        &main_lines,
    );

    // Sem cancelamento: espera os readers normalmente (eles só terminam
    // quando os pipes fecham, ou seja, quando o processo sai). Com
    // cancelamento: corre os readers contra o sinal — se o sinal vencer,
    // mata o processo (fecha os pipes, os readers terminam sozinhos) em vez
    // de esperar `read_stdout`/`read_stderr` até o fim (que só terminariam
    // quando o processo sair por conta própria — exatamente o que o
    // cancelamento quer evitar).
    let cancel = async {
        match cancel_rx {
            Some(mut rx) => wait_for_cancel(&mut rx).await,
            None => std::future::pending().await,
        }
    };
    let mut cancelado = false;
    let mut travado: Option<(String, i32)> = None;
    tokio::select! {
        _ = async { tokio::join!(read_stdout, read_stderr) } => {}
        _ = cancel => {
            cancelado = true;
            group.kill_now();
            let _ = child.start_kill();
        }
        parado = wait_for_stuck_compose(project_name, main_service) => {
            travado = Some(parado);
            group.kill_now();
            let _ = child.start_kill();
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| anyhow!("falha ao aguardar: {e}"))?;
    // Saiu sozinho: o grupo já se foi e o pgid não deve ser reutilizado depois.
    group.disarm();

    if cancelado {
        return Ok(CANCELLED_EXIT_CODE);
    }

    if let Some((container_id, exit_code)) = travado {
        // O compose não transmitiu a saída do container: busca no motor.
        if main_lines.load(Ordering::Relaxed) == 0 {
            for line in container_log_lines(&container_id, 200).await {
                record_job_line(
                    bus,
                    db,
                    job_id,
                    job_run_id,
                    mirror,
                    crate::protocol::LogStream::Stdout,
                    line,
                )
                .await;
            }
        }
        record_job_line(
            bus,
            db,
            job_id,
            job_run_id,
            mirror,
            crate::protocol::LogStream::Stderr,
            format!(
                "==> {main_service} terminou com código {exit_code}, mas o docker compose não saiu em {}s; \
                 o rustploy encerrou o compose e usou o código do container",
                EXIT_GRACE.as_secs()
            ),
        )
        .await;
        return Ok(exit_code);
    }

    // `--exit-code-from` propaga o exit code de `main_service` como o do
    // processo `docker compose` — sem código (ex.: morto por sinal) conta
    // como falha (-1), não sucesso silencioso.
    let exit_code = status.code().unwrap_or(-1);

    // Na falha, o fim da saída do próprio compose entra no log: sem isso,
    // um stack que quebra antes do `main_service` rodar (build, porta
    // ocupada, container com nome repetido) termina com exit code != 0 e
    // log vazio. No sucesso fica de fora, pra não poluir "Ver logs".
    if exit_code != 0 {
        let tail = std::mem::take(&mut *compose_tail.lock().unwrap_or_else(|e| e.into_inner()));
        if !tail.is_empty() {
            let header = format!(
                "==> Job falhou (exit code {exit_code}); últimas {} linhas do docker compose:",
                tail.len()
            );
            record_job_line(
                bus,
                db,
                job_id,
                job_run_id,
                mirror,
                crate::protocol::LogStream::Stderr,
                header,
            )
            .await;
            for line in tail {
                record_job_line(
                    bus,
                    db,
                    job_id,
                    job_run_id,
                    mirror,
                    crate::protocol::LogStream::Stderr,
                    line,
                )
                .await;
            }
        }
    }

    Ok(exit_code)
}

#[cfg(test)]
mod exit_watch_tests {
    use super::*;

    /// O defeito real: `podman compose` roda o plugin como neto, e matar só o filho
    /// deixava o neto vivo. Aqui um `sh` (filho) dispara um `sleep` (neto) no mesmo grupo.
    #[cfg(unix)]
    #[tokio::test]
    async fn guarda_mata_tambem_o_neto_do_compose() {
        // Duração única = marca do neto para o `pgrep` (o `sh` do sistema não tem `exec -a`).
        let segundos = 300_000 + (std::process::id() % 100_000);
        let marca = format!("^sleep {segundos}$");
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", &format!("sleep {segundos} & wait")])
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let mut group = ComposeGroupGuard::new(child.id());
        let vivo = || {
            std::process::Command::new("pgrep")
                .args(["-f", &marca])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        for _ in 0..50 {
            if vivo() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(vivo(), "o neto deveria estar rodando");

        group.kill_now();
        let _ = child.start_kill();
        let _ = child.wait().await;
        for _ in 0..50 {
            if !vivo() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!vivo(), "o neto sobreviveu ao fim do compose");
    }

    #[test]
    fn exited_e_dead_contam_como_saida() {
        assert_eq!(
            parse_container_state("abc", "exited|0\n"),
            MainServiceState::Exited {
                container_id: "abc".into(),
                exit_code: 0
            }
        );
        assert_eq!(
            parse_container_state("abc", "dead|137"),
            MainServiceState::Exited {
                container_id: "abc".into(),
                exit_code: 137
            }
        );
        assert_eq!(
            parse_container_state("abc", "stopped|3"),
            MainServiceState::Exited {
                container_id: "abc".into(),
                exit_code: 3
            }
        );
    }

    #[test]
    fn rodando_ou_saida_ilegivel_nao_e_saida() {
        assert_eq!(
            parse_container_state("abc", "running|0"),
            MainServiceState::NotExited
        );
        assert_eq!(
            parse_container_state("abc", "created|0"),
            MainServiceState::NotExited
        );
        assert_eq!(
            parse_container_state("abc", ""),
            MainServiceState::NotExited
        );
        assert_eq!(
            parse_container_state("abc", "exited|"),
            MainServiceState::NotExited
        );
    }

    #[test]
    fn so_declara_travado_depois_da_carencia() {
        let grace = Duration::from_secs(30);
        let t0 = Instant::now();
        let saiu = MainServiceState::Exited {
            container_id: "c".into(),
            exit_code: 0,
        };
        let mut w = ExitWatch::default();
        assert!(!w.observe(&MainServiceState::NotExited, t0, grace));
        assert!(
            !w.observe(&saiu, t0 + Duration::from_secs(5), grace),
            "acabou de sair"
        );
        assert!(
            !w.observe(&saiu, t0 + Duration::from_secs(30), grace),
            "25 s depois ainda é carência"
        );
        assert!(
            w.observe(&saiu, t0 + Duration::from_secs(35), grace),
            "30 s saído: travado"
        );
    }

    #[test]
    fn container_que_volta_a_rodar_zera_a_contagem() {
        let grace = Duration::from_secs(30);
        let t0 = Instant::now();
        let saiu = MainServiceState::Exited {
            container_id: "c".into(),
            exit_code: 1,
        };
        let mut w = ExitWatch::default();
        assert!(!w.observe(&saiu, t0, grace));
        assert!(!w.observe(
            &MainServiceState::NotExited,
            t0 + Duration::from_secs(20),
            grace
        ));
        assert!(
            !w.observe(&saiu, t0 + Duration::from_secs(40), grace),
            "recomeçou a contar"
        );
        assert!(w.observe(&saiu, t0 + Duration::from_secs(70), grace));
    }
}

#[cfg(test)]
mod warm_args_tests {
    use super::warm_build_args;

    #[test]
    fn build_normal_usa_o_cache() {
        let args = warm_build_args("warm-job_1", "docker-compose.yml", false);
        assert_eq!(args.last().map(String::as_str), Some("build"));
        assert!(!args.iter().any(|a| a == "--no-cache" || a == "--pull"));
    }

    #[test]
    fn build_forcado_refaz_tudo_e_rebaixa_a_base() {
        let args = warm_build_args("warm-job_1", "docker-compose.yml", true);
        assert!(args.iter().any(|a| a == "--no-cache"));
        assert!(args.iter().any(|a| a == "--pull"));
        // As opções vêm depois do subcomando `build`.
        let build = args.iter().position(|a| a == "build").unwrap();
        assert!(args.iter().position(|a| a == "--no-cache").unwrap() > build);
    }
}
