//! `ApplyUpdate`: baixa o pacote assinado e o entrega ao helper root `rustployd-update`.
//!
//! O daemon **não instala nada** e **não precisa ser confiável** para isso: ele só baixa dois
//! arquivos para `/var/lib/rustploy/updates/` e pede ao helper, por socket, que os instale.
//! O helper copia, confere a assinatura e decide (ver `src/bin/rustployd-update/`).
//!
//! O `dpkg` reinicia o daemon no meio, então o resultado final **não** volta pela conexão com
//! o helper: ele está em `/var/lib/rustploy-update/last.json`, que [`UpdateApplier::status`] lê.

use super::github_release::LatestRelease;
use crate::{
    platform::{config::UpdateConfig, https_client},
    store::Db,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use shared::{UpdateProgress, UpdateStage};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use update_core::{
    outcome::{OutcomeKind, UpdateOutcome, read_outcome},
    release::{debian_architecture, select_package_assets},
};

/// Onde o helper escuta (`rustployd-update.socket`).
const HELPER_SOCKET: &str = "/run/rustploy/update.sock";
/// Único diretório de onde o helper aceita um `.deb`; o daemon escreve aqui.
const UPDATES_DIR: &str = "/var/lib/rustploy/updates";
/// Resultado da última instalação, gravado pelo helper (legível por todos).
const OUTCOME_FILE: &str = "/var/lib/rustploy-update/last.json";
/// Os arquivos de uma release do GitHub saem de github.com e redirecionam para `*.githubusercontent.com`.
const DOWNLOAD_HOSTS: &[&str] = &["github.com", "githubusercontent.com"];
const MAX_DEB_BYTES: u64 = 200 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 8 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const HELPER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(60);
/// Quantos snapshots do banco ficam (os mais recentes).
const SNAPSHOTS_KEPT: usize = 3;
/// Um "instalando" mais velho que isto é resto de um helper que morreu, não uma instalação viva.
const STALE_IN_PROGRESS_SECS: u64 = 30 * 60;

#[derive(Clone)]
pub struct HelperPaths {
    pub socket: PathBuf,
    pub updates_dir: PathBuf,
    pub outcome_file: PathBuf,
}

impl Default for HelperPaths {
    fn default() -> Self {
        Self {
            socket: HELPER_SOCKET.into(),
            updates_dir: UPDATES_DIR.into(),
            outcome_file: OUTCOME_FILE.into(),
        }
    }
}

#[derive(Clone)]
struct ApplyState {
    stage: UpdateStage,
    target_version: Option<String>,
    message: String,
    started_unix: u64,
    finished_at: Option<DateTime<Utc>>,
}

impl ApplyState {
    fn idle() -> Self {
        Self {
            stage: UpdateStage::Idle,
            target_version: None,
            message: String::new(),
            started_unix: 0,
            finished_at: None,
        }
    }
}

pub struct UpdateApplier {
    paths: HelperPaths,
    allow_agent_apply: bool,
    state: Mutex<ApplyState>,
}

/// Tudo de que a tarefa de fundo precisa (ela sobrevive ao handler que a disparou).
pub struct ApplyPlan {
    pub release: LatestRelease,
    pub repo: String,
    pub db: Arc<Db>,
    pub snapshots_dir: PathBuf,
    pub current_version: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

impl UpdateApplier {
    pub fn new(config: &UpdateConfig) -> Self {
        Self::with_paths(config, HelperPaths::default())
    }

    pub fn with_paths(config: &UpdateConfig, paths: HelperPaths) -> Self {
        Self {
            paths,
            allow_agent_apply: config.allow_agent_apply,
            state: Mutex::new(ApplyState::idle()),
        }
    }

    /// `[update] allow_agent_apply`: se o agente MCP pode pedir `ApplyUpdate`/`UpdateRollback`.
    pub fn allow_agent_apply(&self) -> bool {
        self.allow_agent_apply
    }

    /// O helper existe nesta máquina? (Sem systemd ou instalação por `cargo`, o socket não existe.)
    pub fn is_helper_available(&self) -> bool {
        self.paths.socket.exists()
    }

    /// Marca o início de uma tentativa. Falha se já há uma em curso (só uma por vez).
    pub fn try_begin(&self, target_version: &str) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(
            state.stage,
            UpdateStage::Downloading | UpdateStage::Installing
        ) {
            return Err("já há uma atualização em andamento".into());
        }
        *state = ApplyState {
            stage: UpdateStage::Downloading,
            target_version: Some(target_version.to_string()),
            message: "preparando".into(),
            started_unix: now_unix(),
            finished_at: None,
        };
        Ok(())
    }

    fn set(&self, stage: UpdateStage, message: impl Into<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stage = stage;
        state.message = message.into();
        if matches!(
            stage,
            UpdateStage::Succeeded | UpdateStage::Failed | UpdateStage::RolledBack
        ) {
            state.finished_at = Some(Utc::now());
        }
    }

    /// O que a UI mostra: o passo em memória e, depois do reinício (ou quando o helper já
    /// terminou), o que o helper gravou em `last.json`.
    pub fn status(&self, running_version: &str) -> UpdateProgress {
        let memory = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let outcome = read_outcome(&self.paths.outcome_file).ok().flatten();
        let is_helper_available = self.is_helper_available();

        let (stage, target_version, message, finished_at) = match memory.stage {
            UpdateStage::Downloading => (memory.stage, memory.target_version, memory.message, None),
            UpdateStage::Installing | UpdateStage::RollingBack => {
                // O helper pode ter terminado (e o daemon sobrevivido: falhou ou voltou atrás).
                match outcome.filter(|outcome| outcome.at_unix >= memory.started_unix) {
                    Some(outcome) => {
                        from_outcome(&outcome, memory.target_version, running_version, true)
                    }
                    None => (memory.stage, memory.target_version, memory.message, None),
                }
            }
            UpdateStage::Succeeded | UpdateStage::Failed | UpdateStage::RolledBack => (
                memory.stage,
                memory.target_version,
                memory.message,
                memory.finished_at,
            ),
            UpdateStage::Idle => match outcome {
                Some(outcome) => from_outcome(&outcome, None, running_version, false),
                None => (UpdateStage::Idle, None, String::new(), None),
            },
        };
        UpdateProgress {
            stage,
            target_version,
            message,
            is_helper_available,
            finished_at,
        }
    }

    /// Roda a atualização inteira em segundo plano; qualquer erro vira `Failed` com a causa.
    pub async fn run(&self, plan: ApplyPlan) {
        if let Err(message) = self.run_steps(&plan).await {
            tracing::warn!(%message, "atualização do daemon falhou");
            self.set(UpdateStage::Failed, message);
        }
    }

    async fn run_steps(&self, plan: &ApplyPlan) -> Result<(), String> {
        self.set(UpdateStage::Downloading, "tirando um snapshot do banco");
        snapshot_database(&plan.db, &plan.snapshots_dir, &plan.current_version)
            .await
            .map_err(|error| format!("snapshot do banco: {error}"))?;

        let (deb_asset, signature_asset) = select_package_assets(
            &plan.release.assets,
            &plan.repo,
            "rustployd",
            debian_architecture()?,
            &plan.release.tag,
        )?;
        self.set(
            UpdateStage::Downloading,
            format!("baixando {}", deb_asset.name),
        );
        prepare_updates_dir(&self.paths.updates_dir)
            .map_err(|error| format!("diretório de atualizações: {error}"))?;
        let deb_path = self.paths.updates_dir.join(&deb_asset.name);
        let signature_path = self.paths.updates_dir.join(&signature_asset.name);
        let user_agent = format!("rustployd/{}", env!("CARGO_PKG_VERSION"));
        let headers = [("user-agent", user_agent.as_str())];
        https_client::download(
            &deb_asset.url,
            &headers,
            &deb_path,
            MAX_DEB_BYTES,
            DOWNLOAD_TIMEOUT,
            DOWNLOAD_HOSTS,
        )
        .await
        .map_err(|error| format!("download do pacote: {error}"))?;
        https_client::download(
            &signature_asset.url,
            &headers,
            &signature_path,
            MAX_SIGNATURE_BYTES,
            DOWNLOAD_TIMEOUT,
            DOWNLOAD_HOSTS,
        )
        .await
        .map_err(|error| format!("download da assinatura: {error}"))?;

        self.set(UpdateStage::Installing, "pedindo a instalação ao helper");
        // O helper é quem decide: copia, confere a assinatura e as regras. Se recusar, a causa volta aqui.
        let request = serde_json::json!({
            "op": "install",
            "deb": deb_path.to_string_lossy(),
            "sig": signature_path.to_string_lossy(),
        });
        let reply = helper_call(&self.paths.socket, &request, HELPER_ACCEPT_TIMEOUT).await?;
        if !reply.ok {
            return Err(reply
                .error
                .unwrap_or_else(|| "o helper recusou o pacote".into()));
        }
        self.set(
            UpdateStage::Installing,
            "instalando; o daemon vai reiniciar",
        );
        Ok(())
    }

    /// `UpdateRollback`: pede ao helper que restaure os binários do último backup.
    pub async fn rollback(&self) -> Result<(), String> {
        if !self.is_helper_available() {
            return Err("o helper de atualização não está disponível nesta máquina".into());
        }
        {
            // Marca antes de pedir: o `last.json` de uma atualização anterior não pode ser lido
            // como o resultado deste rollback.
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *state = ApplyState {
                stage: UpdateStage::RollingBack,
                target_version: None,
                message: "revertendo os binários".into(),
                started_unix: now_unix(),
                finished_at: None,
            };
        }
        // O helper responde "aceito" antes de reiniciar o daemon; o resultado vai para o last.json.
        let reply = match helper_call(
            &self.paths.socket,
            &serde_json::json!({ "op": "rollback" }),
            HELPER_ACCEPT_TIMEOUT,
        )
        .await
        {
            Ok(reply) => reply,
            Err(message) => {
                self.set(UpdateStage::Failed, message.clone());
                return Err(message);
            }
        };
        if reply.ok {
            Ok(())
        } else {
            let message = reply
                .error
                .unwrap_or_else(|| "o helper recusou reverter".into());
            self.set(UpdateStage::Failed, message.clone());
            Err(message)
        }
    }
}

fn from_outcome(
    outcome: &UpdateOutcome,
    target_version: Option<String>,
    running_version: &str,
    from_this_attempt: bool,
) -> (UpdateStage, Option<String>, String, Option<DateTime<Utc>>) {
    let finished_at = i64::try_from(outcome.at_unix)
        .ok()
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
    let target = target_version
        .or_else(|| Some(outcome.version.clone()).filter(|version| !version.is_empty()));
    match outcome.kind {
        OutcomeKind::InProgress => {
            let is_stale = now_unix().saturating_sub(outcome.at_unix) > STALE_IN_PROGRESS_SECS;
            if is_stale && !from_this_attempt {
                (UpdateStage::Idle, None, String::new(), None)
            } else {
                (
                    UpdateStage::Installing,
                    target,
                    outcome.message.clone(),
                    None,
                )
            }
        }
        OutcomeKind::Installed => {
            // Um "instalado" só é notícia se o que está rodando agora é essa versão: um resultado
            // antigo (de duas versões atrás) não deve reaparecer como se fosse o de hoje.
            if from_this_attempt || package_matches_running(&outcome.version, running_version) {
                (
                    UpdateStage::Succeeded,
                    target,
                    outcome.message.clone(),
                    finished_at,
                )
            } else {
                (UpdateStage::Idle, None, String::new(), None)
            }
        }
        OutcomeKind::Failed => (
            UpdateStage::Failed,
            target,
            outcome.message.clone(),
            finished_at,
        ),
        OutcomeKind::RolledBack => (
            UpdateStage::RolledBack,
            target,
            outcome.message.clone(),
            finished_at,
        ),
    }
}

/// `0.1.10-1` (versão do pacote) casa com `0.1.10` (versão do crate em execução).
fn package_matches_running(package_version: &str, running_version: &str) -> bool {
    let upstream = package_version
        .rsplit_once('-')
        .map(|(upstream, _revision)| upstream)
        .unwrap_or(package_version);
    upstream == running_version
}

/// Cria o diretório e tira o que sobrou de tentativas anteriores (pacote velho não pode ser
/// confundido com o novo).
fn prepare_updates_dir(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    for entry in std::fs::read_dir(directory)?.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Cópia consistente do banco **em uso** (`VACUUM INTO`), antes de a versão nova poder migrar o
/// esquema: voltar o binário antigo só serve se houver um banco do tempo dele. Mantém os mais recentes.
pub async fn snapshot_database(db: &Db, directory: &Path, tag: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let safe_tag: String = tag
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || ".-+~".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    let target = directory.join(format!("pre-update-{safe_tag}-{}.db", now_unix()));
    // O VACUUM INTO se recusa a sobrescrever: um snapshot do mesmo segundo precisa de outro nome.
    let target = unique_path(target);
    sqlx::query("VACUUM INTO ?1")
        .bind(target.to_string_lossy().into_owned())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    prune_snapshots(directory);
    Ok(target)
}

fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    (1..)
        .map(|attempt| path.with_extension(format!("{attempt}.db")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(path)
}

fn prune_snapshots(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut snapshots: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("pre-update-")
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    snapshots.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, path) in snapshots.into_iter().skip(SNAPSHOTS_KEPT) {
        let _ = std::fs::remove_file(path);
    }
}

/// Um helper que cai no meio (ou fecha antes de ler) aparece como conexão resetada/quebrada:
/// para quem lê é o mesmo caso de "fechou sem responder", e não um erro de rede a investigar.
fn helper_io_error(error: std::io::Error) -> String {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset, UnexpectedEof};
    match error.kind() {
        ConnectionReset | BrokenPipe | UnexpectedEof => {
            "o helper fechou a conexão sem responder".to_string()
        }
        _ => format!("helper: {error}"),
    }
}

#[derive(Debug, Deserialize)]
struct HelperReply {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Uma linha JSON para o helper e uma de volta (ver `src/bin/rustployd-update/protocol.rs`).
async fn helper_call(
    socket: &Path,
    request: &serde_json::Value,
    timeout: Duration,
) -> Result<HelperReply, String> {
    let exchange = async {
        let stream = UnixStream::connect(socket).await.map_err(|error| {
            format!(
                "não consegui falar com o helper de atualização ({}): {error}",
                socket.display()
            )
        })?;
        let (reader, mut writer) = stream.into_split();
        let mut line = request.to_string();
        line.push('\n');
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(helper_io_error)?;
        let mut reply = String::new();
        BufReader::new(reader)
            .read_line(&mut reply)
            .await
            .map_err(helper_io_error)?;
        if reply.trim().is_empty() {
            return Err("o helper fechou a conexão sem responder".to_string());
        }
        serde_json::from_str::<HelperReply>(reply.trim())
            .map_err(|error| format!("resposta ilegível do helper: {error}"))
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| "o helper de atualização não respondeu a tempo".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use update_core::outcome::write_outcome;

    #[test]
    fn package_version_matches_the_crate_version() {
        assert!(package_matches_running("0.1.10-1", "0.1.10"));
        assert!(!package_matches_running("0.1.10-1", "0.1.9"));
        assert!(!package_matches_running("0.1.9-1", "0.1.10"));
        assert!(package_matches_running("0.1.10", "0.1.10"));
    }

    fn applier(directory: &Path) -> UpdateApplier {
        UpdateApplier::with_paths(
            &UpdateConfig::default(),
            HelperPaths {
                socket: directory.join("update.sock"),
                updates_dir: directory.join("updates"),
                outcome_file: directory.join("last.json"),
            },
        )
    }

    fn outcome(directory: &Path, version: &str, kind: OutcomeKind, message: &str, age_secs: u64) {
        let mut outcome = UpdateOutcome::new("rustployd", version, kind, message);
        outcome.at_unix = now_unix() - age_secs;
        write_outcome(&directory.join("last.json"), &outcome).unwrap();
    }

    #[test]
    fn idle_with_no_history_says_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let status = applier(directory.path()).status("0.1.10");
        assert_eq!(status.stage, UpdateStage::Idle);
        assert!(status.message.is_empty());
        assert!(!status.is_helper_available);
    }

    #[test]
    fn after_a_restart_a_matching_install_is_reported_as_success() {
        let directory = tempfile::tempdir().unwrap();
        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::Installed,
            "instalado",
            5,
        );
        let status = applier(directory.path()).status("0.1.10");
        assert_eq!(status.stage, UpdateStage::Succeeded);
        assert_eq!(status.target_version.as_deref(), Some("0.1.10-1"));
        assert!(status.finished_at.is_some());
    }

    #[test]
    fn an_old_install_result_is_not_news_after_a_later_version_runs() {
        let directory = tempfile::tempdir().unwrap();
        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::Installed,
            "instalado",
            99_999,
        );
        assert_eq!(
            applier(directory.path()).status("0.1.11").stage,
            UpdateStage::Idle
        );
    }

    #[test]
    fn failures_and_rollbacks_carry_their_cause() {
        let directory = tempfile::tempdir().unwrap();
        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::Failed,
            "assinatura inválida",
            5,
        );
        let failed = applier(directory.path()).status("0.1.9");
        assert_eq!(failed.stage, UpdateStage::Failed);
        assert_eq!(failed.message, "assinatura inválida");

        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::RolledBack,
            "serviço caiu",
            5,
        );
        assert_eq!(
            applier(directory.path()).status("0.1.9").stage,
            UpdateStage::RolledBack
        );
    }

    #[test]
    fn a_live_in_progress_marker_is_installing_but_a_stale_one_is_not() {
        let directory = tempfile::tempdir().unwrap();
        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::InProgress,
            "instalando",
            10,
        );
        assert_eq!(
            applier(directory.path()).status("0.1.9").stage,
            UpdateStage::Installing
        );

        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::InProgress,
            "instalando",
            STALE_IN_PROGRESS_SECS + 60,
        );
        assert_eq!(
            applier(directory.path()).status("0.1.9").stage,
            UpdateStage::Idle
        );
    }

    #[test]
    fn only_one_attempt_runs_at_a_time_and_a_finished_one_can_be_retried() {
        let directory = tempfile::tempdir().unwrap();
        let applier = applier(directory.path());
        applier.try_begin("0.1.10").unwrap();
        assert!(applier.try_begin("0.1.10").is_err());
        assert_eq!(applier.status("0.1.9").stage, UpdateStage::Downloading);

        applier.set(UpdateStage::Failed, "sem rede");
        let status = applier.status("0.1.9");
        assert_eq!(status.stage, UpdateStage::Failed);
        assert_eq!(status.message, "sem rede");
        assert!(status.finished_at.is_some());
        assert!(applier.try_begin("0.1.10").is_ok());
    }

    #[test]
    fn while_installing_the_helpers_verdict_for_this_attempt_wins() {
        let directory = tempfile::tempdir().unwrap();
        let applier = applier(directory.path());
        applier.try_begin("0.1.10").unwrap();
        applier.set(
            UpdateStage::Installing,
            "instalando; o daemon vai reiniciar",
        );
        assert_eq!(applier.status("0.1.9").stage, UpdateStage::Installing);

        // O daemon sobreviveu e o helper voltou atrás: a UI precisa saber.
        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::RolledBack,
            "serviço caiu",
            0,
        );
        let status = applier.status("0.1.9");
        assert_eq!(status.stage, UpdateStage::RolledBack);
        assert_eq!(status.message, "serviço caiu");
    }

    #[tokio::test]
    async fn a_rollback_ignores_the_result_of_an_older_install_until_its_own_arrives() {
        let directory = tempfile::tempdir().unwrap();
        // Resultado de uma instalação anterior, bem-sucedida e de uma versão que ainda roda.
        outcome(
            directory.path(),
            "0.1.10-1",
            OutcomeKind::Installed,
            "instalado",
            120,
        );

        // Um helper que aceita o pedido de rollback (sem executar nada).
        let socket = directory.path().join("update.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut request = String::new();
            BufReader::new(reader)
                .read_line(&mut request)
                .await
                .unwrap();
            writer.write_all(b"{\"ok\":true}\n").await.unwrap();
        });
        let applier = applier(directory.path());

        applier.rollback().await.unwrap();

        // O "instalado" antigo NÃO pode aparecer como o resultado desta reversão.
        let status = applier.status("0.1.10");
        assert_eq!(status.stage, UpdateStage::RollingBack);

        // Quando o helper grava o resultado do rollback, ele passa a valer.
        outcome(
            directory.path(),
            "0.1.9-1",
            OutcomeKind::RolledBack,
            "restaurado",
            0,
        );
        let status = applier.status("0.1.9");
        assert_eq!(status.stage, UpdateStage::RolledBack);
        assert_eq!(status.message, "restaurado");
    }

    #[tokio::test]
    async fn a_refused_rollback_becomes_a_failed_status_with_the_reason() {
        let directory = tempfile::tempdir().unwrap();
        let (socket, _task) = fake_helper(
            directory.path(),
            "{\"ok\":false,\"error\":\"não há backup\"}\n",
        )
        .await;
        let mut paths = HelperPaths::default();
        paths.socket = socket;
        paths.outcome_file = directory.path().join("last.json");
        let applier = UpdateApplier::with_paths(&UpdateConfig::default(), paths);

        assert_eq!(applier.rollback().await.unwrap_err(), "não há backup");
        let status = applier.status("0.1.9");
        assert_eq!(status.stage, UpdateStage::Failed);
        assert_eq!(status.message, "não há backup");
    }

    #[test]
    fn leftovers_in_the_updates_dir_are_cleared() {
        let directory = tempfile::tempdir().unwrap();
        let updates = directory.path().join("updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join("rustployd_0.1.9-1_amd64.deb"), b"velho").unwrap();
        std::fs::write(updates.join("x.deb.partial"), b"meio").unwrap();
        prepare_updates_dir(&updates).unwrap();
        assert_eq!(std::fs::read_dir(&updates).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn the_database_snapshot_is_a_real_database_and_only_the_latest_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        let db = crate::store::connect(&directory.path().join("db"))
            .await
            .unwrap();
        let snapshots = directory.path().join("snapshots");

        let mut last = None;
        for _ in 0..5 {
            last = Some(
                snapshot_database(&db, &snapshots, "0.1.9/../x")
                    .await
                    .unwrap(),
            );
            // O mtime tem resolução fina, mas garante a ordem entre arquivos criados em sequência.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let kept = std::fs::read_dir(&snapshots).unwrap().count();
        assert_eq!(kept, SNAPSHOTS_KEPT);

        // O snapshot abre como SQLite e tem as tabelas do banco original.
        let last = last.unwrap();
        assert!(!last.file_name().unwrap().to_string_lossy().contains('/'));
        let copy = sqlx::sqlite::SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}", last.display()))
            .await
            .unwrap();
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'service'",
        )
        .fetch_one(&copy)
        .await
        .unwrap();
        assert_eq!(tables, 1);
    }

    /// Conversa com um "helper" de mentira por um socket Unix de verdade.
    async fn fake_helper(
        directory: &Path,
        reply: &'static str,
    ) -> (PathBuf, tokio::task::JoinHandle<String>) {
        let socket = directory.join("helper.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut request = String::new();
            BufReader::new(reader)
                .read_line(&mut request)
                .await
                .unwrap();
            writer.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        (socket, task)
    }

    #[tokio::test]
    async fn the_helper_request_is_one_json_line_with_the_two_paths() {
        let directory = tempfile::tempdir().unwrap();
        let (socket, task) =
            fake_helper(directory.path(), "{\"ok\":true,\"version\":\"0.1.10-1\"}\n").await;
        let request =
            serde_json::json!({"op": "install", "deb": "/a.deb", "sig": "/a.deb.minisig"});

        let reply = helper_call(&socket, &request, Duration::from_secs(5))
            .await
            .unwrap();

        assert!(reply.ok);
        let received = task.await.unwrap();
        assert_eq!(received.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(received.trim()).unwrap();
        assert_eq!(parsed["op"], "install");
        assert_eq!(parsed["deb"], "/a.deb");
    }

    #[tokio::test]
    async fn a_refusal_from_the_helper_surfaces_its_reason() {
        let directory = tempfile::tempdir().unwrap();
        let (socket, _task) = fake_helper(
            directory.path(),
            "{\"ok\":false,\"error\":\"assinatura inválida\"}\n",
        )
        .await;
        let reply = helper_call(
            &socket,
            &serde_json::json!({"op": "rollback"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("assinatura inválida"));
    }

    #[tokio::test]
    async fn a_missing_or_silent_helper_is_a_clear_error() {
        let directory = tempfile::tempdir().unwrap();
        let request = serde_json::json!({"op": "rollback"});

        let missing = helper_call(
            &directory.path().join("nao-existe.sock"),
            &request,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            missing.contains("não consegui falar com o helper"),
            "{missing}"
        );

        // Aceita a conexão e fecha sem responder.
        let socket = directory.path().join("mudo.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let silent = helper_call(&socket, &request, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(silent.contains("sem responder"), "{silent}");
    }
}
