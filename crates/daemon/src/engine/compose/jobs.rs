//! Execução de jobs one-shot via compose: arquivos, imagens, aquecimento e cancelamento.

use crate::platform::{event_bus::EventBus, log_sink::LogSink};
use anyhow::{Result, anyhow};
use bollard::Docker;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
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
#[allow(clippy::too_many_arguments)]
pub async fn warm(
    source: JobBuildSource<'_>,
    project_name: &str,
    network_name: &str,
    env_vars: &[(String, String)],
    build_dir: &Path,
    registry_internal_token: Option<Arc<str>>,
    docker: &Docker,
) -> Result<ComposeImages> {
    info!(project = %project_name, "compose_warm: iniciando aquecimento");
    let files = write_job_files(source, network_name, env_vars, build_dir).await?;

    if let Some(token) = &registry_internal_token
        && let Err(e) = registry_login(token).await
    {
        tracing::warn!(error = %e, project = %project_name, "compose_warm: falha ao autenticar no registry embutido, pull vai falhar se a imagem for de lá");
    }

    let result = warm_images(project_name, &files.compose_rel_path, build_dir, docker).await;

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
) -> Result<ComposeImages> {
    let images = resolve_compose_images(project_name, compose_rel_path, build_dir).await?;

    for image in &images.pulled_images {
        if docker.inspect_image(image).await.is_ok() {
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
            .args([
                "compose",
                "-p",
                project_name,
                "-f",
                compose_rel_path,
                "--env-file",
                ".env",
                "build",
            ])
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

/// Roda o `docker compose up` de um job até o `main_service` sair, transmitindo
/// stdout/stderr para o log e matando o processo se chegar o sinal de
/// cancelamento.
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
    let mut child = crate::engine::cli()
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
        .spawn()
        .map_err(|e| anyhow!("falha ao iniciar docker compose: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mirror = mirror_deployment.as_ref();
    let compose_tail = Mutex::new(VecDeque::new());
    let read_stdout = read_compose_output(
        stdout,
        shared::protocol::LogStream::Stdout,
        main_service,
        bus,
        db,
        job_id,
        job_run_id,
        mirror,
        &compose_tail,
    );
    let read_stderr = read_compose_output(
        stderr,
        shared::protocol::LogStream::Stderr,
        main_service,
        bus,
        db,
        job_id,
        job_run_id,
        mirror,
        &compose_tail,
    );

    // Sem cancelamento: espera os readers normalmente (eles só terminam
    // quando os pipes fecham, ou seja, quando o processo sai). Com
    // cancelamento: corre os readers contra o sinal — se o sinal vencer,
    // mata o processo (fecha os pipes, os readers terminam sozinhos) em vez
    // de esperar `read_stdout`/`read_stderr` até o fim (que só terminariam
    // quando o processo sair por conta própria — exatamente o que o
    // cancelamento quer evitar).
    let mut cancelado = false;
    match cancel_rx {
        Some(mut rx) => {
            tokio::select! {
                _ = async { tokio::join!(read_stdout, read_stderr) } => {}
                _ = wait_for_cancel(&mut rx) => {
                    cancelado = true;
                    let _ = child.start_kill();
                }
            }
        }
        None => {
            tokio::join!(read_stdout, read_stderr);
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| anyhow!("falha ao aguardar: {e}"))?;

    if cancelado {
        return Ok(CANCELLED_EXIT_CODE);
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
                shared::protocol::LogStream::Stderr,
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
                    shared::protocol::LogStream::Stderr,
                    line,
                )
                .await;
            }
        }
    }

    Ok(exit_code)
}
