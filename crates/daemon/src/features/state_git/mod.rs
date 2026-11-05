//! Sincronização do estado de infra com um repositório git (Fase 6 do plano
//! `docs/plano-backup-estado-em-git.md`, seção R6).
//!
//! Três modos, guardados no SQL (`daemon_settings`, runtime — nunca no YAML que vai ao git):
//! - `off` (padrão): o YAML só existe no disco;
//! - `commit`: commita localmente a cada mudança (com debounce), sem rede;
//! - `sync`: commit + `push` para o remoto. (O `fetch`/`pull` é da Fase 7.)
//!
//! **A trava:** ligar só é permitido se a auditoria ([`crate::store::infra::audit`]) não achar valor
//! em claro, e **a cada commit** ela roda de novo, *dentro do lock de escrita* do repositório. Se
//! aparecer algo (o usuário moveu um secret de volta para env, por exemplo), o commit é pulado e a
//! situação fica `blocked_by_plaintext` até limpar. Assim o histórico nunca contém um segredo em
//! claro — o que importa, porque apagar segredo do histórico do git não funciona de verdade.
//!
//! Falha de rede/credencial no `push` **não derruba nem bloqueia nada**: o erro vai para o status e
//! o daemon tenta de novo no próximo ciclo.

pub mod git;

use crate::features::git_providers;
use crate::platform::secrets::SecretsManager;
use crate::store::infra::audit::{Certainty, Finding, plaintext_findings};
use crate::store::infra::model::InfraState;
use crate::store::{Db, daemon_settings};
use git::{Auth, CommitInfo};
use shared::{StateGitCommit, StateGitRejected, StateGitRemoved, StateGitStatus};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

const KEY_MODE: &str = "state_git_mode";
const KEY_REMOTE: &str = "state_git_remote";
const KEY_BRANCH: &str = "state_git_branch";
const KEY_PROVIDER: &str = "state_git_provider_id";
const KEY_ALLOW: &str = "state_git_audit_allow";
const DEFAULT_BRANCH: &str = "main";
const DEBOUNCE: Duration = Duration::from_secs(10);
/// Padrão do `fetch` nos testes (em produção vem de `[state_git] fetch_interval_secs`).
#[cfg(test)]
const DEFAULT_FETCH_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Commit,
    Sync,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Commit => "commit",
            Mode::Sync => "sync",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Mode::Off),
            "commit" => Some(Mode::Commit),
            "sync" => Some(Mode::Sync),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub remote: Option<String>,
    pub branch: String,
    pub provider_id: Option<String>,
}

#[derive(thiserror::Error, Debug)]
pub enum StateGitError {
    /// A auditoria achou valores em claro: ligar (ou commitar) é recusado.
    #[error(
        "há {0} valor(es) em claro no estado de infra; mova-os para secrets (veja InfraAudit) antes de ligar a sincronização com o git"
    )]
    Plaintext(usize),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// O que aconteceu ao tentar aplicar um commit vindo de fora.
enum Applied {
    Yes,
    /// Adiado (mudança local ainda não commitada): tenta de novo no próximo ciclo.
    Later,
    /// Recusado, com o motivo.
    No(String),
}

enum Reconciled {
    /// Siga para o push (se houver o que enviar).
    Push,
    /// Nada mais a fazer neste ciclo (já igual, pausado ou aplicado).
    Done,
}

/// O pull precisa esperar: há mudança local ainda não commitada. Não é erro; tenta de novo.
#[derive(thiserror::Error, Debug)]
#[error("mudanças locais ainda não commitadas")]
struct RetryLater;

/// O estado foi aplicado, mas o `git reset` que alinha o repositório falhou.
#[derive(thiserror::Error, Debug)]
#[error("{0}")]
struct GitStuck(String);

#[derive(Default)]
struct Runtime {
    /// A sincronização está pausada (histórico divergiu) até alguém retomar.
    paused: Option<String>,
    /// O último commit do remoto que foi recusado: `(sha, motivo)`.
    incoming_error: Option<(String, String)>,
    last_fetch_at: Option<String>,
    ahead: usize,
    behind: usize,
    last_commit: Option<CommitInfo>,
    last_push_ok: Option<bool>,
    last_push_at: Option<String>,
    last_error: Option<String>,
    blocked: usize,
    needs_push: bool,
}

pub struct StateGit {
    db: Arc<Db>,
    secrets: Arc<SecretsManager>,
    rt: Mutex<Runtime>,
    /// Acorda o worker fora do ritmo normal ("sincronizar agora", suspeita liberada…).
    wake: tokio::sync::Notify,
    /// Um ciclo por vez (worker × "sincronizar agora").
    cycle_lock: tokio::sync::Mutex<()>,
    /// Quanto esperar depois da última mudança antes de commitar.
    debounce: Duration,
    /// De quanto em quanto tempo fazer `fetch` (e repetir o que falhou) sem que nada tenha mudado.
    fetch_every: Duration,
    /// Chamado depois de uma restauração num servidor limpo: sobe o que foi restaurado, na ordem
    /// das dependências (o bootstrap liga isto ao religamento do boot).
    after_restore: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
}

fn fingerprint(project: &str, scope: &str, what: &str) -> String {
    format!("{project}\u{1f}{scope}\u{1f}{what}")
}

/// Achados que **impedem** o commit: as certezas sempre, e as suspeitas que o usuário não liberou.
pub(crate) fn blocking(state: &InfraState, allow: &BTreeSet<String>) -> Vec<Finding> {
    plaintext_findings(state)
        .into_iter()
        .filter(|f| {
            f.certainty == Certainty::Definite
                || !allow.contains(&fingerprint(&f.project, &f.scope, &f.what))
        })
        .collect()
}

/// Valida uma árvore vinda de fora ANTES de aplicar qualquer coisa (R6, itens 1 e 2): sem valor em
/// claro, dependências coerentes, e todo segredo cifrado precisa decifrar com a identidade deste
/// servidor (senão o deploy falharia depois, longe de quem causou).
fn validate_incoming(
    root: &std::path::Path,
    new: &InfraState,
    allow: &BTreeSet<String>,
    secrets: &SecretsManager,
    require_clean: bool,
) -> anyhow::Result<()> {
    // No pull comum, mudança local não commitada adia a aplicação. Ao RESOLVER uma divergência
    // ("usar o do remoto") o local é descartado de propósito (depois de salvo numa branch).
    if require_clean && !git::is_clean(root)? {
        return Err(anyhow::Error::new(RetryLater));
    }
    let bad = blocking(new, allow);
    if !bad.is_empty() {
        let lista: Vec<String> = bad
            .iter()
            .take(5)
            .map(|f| format!("{} / {} / {}", f.project, f.scope, f.what))
            .collect();
        anyhow::bail!(
            "traz {} valor(es) em claro (nada em claro pode entrar): {}{}",
            bad.len(),
            lista.join("; "),
            if bad.len() > 5 { "; …" } else { "" }
        );
    }
    for p in &new.projects {
        let all: Vec<(String, String, Vec<String>)> = p
            .services
            .iter()
            .map(|s| (s.id.clone(), s.spec.name.clone(), s.spec.depends_on.clone()))
            .collect();
        for svc in &p.services {
            crate::features::deploy::deps::validate(&all, Some(&svc.id), &svc.spec.depends_on)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "dependências do serviço '{}' (projeto '{}'): {}",
                        svc.spec.name,
                        p.name,
                        e.message
                    )
                })?;
        }
        let decifra = |what: String, v: &str| -> anyhow::Result<()> {
            if crate::platform::secrets::is_enc(v) {
                secrets.decrypt(v).map(|_| ()).map_err(|_| {
                    anyhow::anyhow!("{what} não decifra com a identidade deste servidor")
                })?;
            }
            Ok(())
        };
        for sec in &p.secrets {
            decifra(
                format!("o secret '{}' do projeto '{}'", sec.key, p.name),
                &sec.value,
            )?;
        }
        for svc in &p.services {
            for sec in &svc.secrets {
                decifra(
                    format!("o secret '{}' do serviço '{}'", sec.key, svc.spec.name),
                    &sec.value,
                )?;
            }
            if let Some(t) = &svc.webhook_token {
                decifra(format!("o webhook do serviço '{}'", svc.spec.name), t)?;
            }
        }
    }
    for prov in &new.providers {
        if let Some(v) = &prov.oauth_client_secret_enc {
            decifra_provider(secrets, &prov.name, v)?;
        }
    }
    Ok(())
}

fn decifra_provider(secrets: &SecretsManager, name: &str, v: &str) -> anyhow::Result<()> {
    if crate::platform::secrets::is_enc(v) {
        secrets.decrypt(v).map(|_| ()).map_err(|_| {
            anyhow::anyhow!(
                "o client_secret do provider '{name}' não decifra com a identidade deste servidor"
            )
        })?;
    }
    Ok(())
}

struct RemovedItem {
    id: String,
    kind: String,
    name: String,
    project: String,
    compose_project: Option<String>,
}

struct StateDiff {
    removed: Vec<RemovedItem>,
    /// Serviços (que existem nos dois lados) cujo declarado mudou.
    spec_changed: Vec<String>,
}

/// O que um `pull` mudou de relevante para quem opera: o que sumiu e o que precisa de redeploy.
fn diff_states(old: &InfraState, new: &InfraState) -> StateDiff {
    let mut removed = vec![];
    let mut spec_changed = vec![];
    for op in &old.projects {
        match new.projects.iter().find(|p| p.id == op.id) {
            None => {
                removed.push(RemovedItem {
                    id: format!("projeto:{}", op.id),
                    kind: "projeto".into(),
                    name: op.name.clone(),
                    project: String::new(),
                    compose_project: None,
                });
                for s in &op.services {
                    removed.push(RemovedItem {
                        id: format!("serviço:{}", s.id),
                        kind: "serviço".into(),
                        name: s.spec.name.clone(),
                        project: op.name.clone(),
                        compose_project: s.compose_project.clone(),
                    });
                }
            }
            Some(np) => {
                // env/secrets de projeto mudaram: todo serviço do projeto precisa de redeploy
                let project_changed = op.env_vars != np.env_vars || op.secrets != np.secrets;
                for s in &op.services {
                    match np.services.iter().find(|x| x.id == s.id) {
                        None => removed.push(RemovedItem {
                            id: format!("serviço:{}", s.id),
                            kind: "serviço".into(),
                            name: s.spec.name.clone(),
                            project: op.name.clone(),
                            compose_project: s.compose_project.clone(),
                        }),
                        Some(ns) => {
                            if project_changed || ns.spec != s.spec || ns.secrets != s.secrets {
                                spec_changed.push(s.id.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    StateDiff {
        removed,
        spec_changed,
    }
}

/// Assunto do commit a partir dos arquivos alterados (nomes legíveis, não ids).
fn subject(paths: &[String], state: &InfraState) -> String {
    let names: BTreeMap<&str, &str> = state
        .projects
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    let mut labels: Vec<String> = vec![];
    for p in paths {
        let label = if let Some(rest) = p.strip_prefix("projects/") {
            let id = rest.split('/').next().unwrap_or(rest);
            format!("projeto {}", names.get(id).copied().unwrap_or(id))
        } else {
            match p.as_str() {
                "settings.yml" => "configurações".into(),
                "providers.yml" => "providers".into(),
                "MANIFEST.json" => "formato".into(),
                other => other.to_string(),
            }
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    let mut s = format!("Atualiza {}", labels.join(", "));
    if s.chars().count() > 100 {
        s = s.chars().take(97).collect::<String>() + "...";
    }
    s
}

impl StateGit {
    pub fn new(db: Arc<Db>, secrets: Arc<SecretsManager>) -> Arc<Self> {
        // `[state_git] fetch_interval_secs` (padrão 60 s; nunca menos de 10 s).
        let secs = crate::platform::config::RustployConfig::global()
            .state_git
            .fetch_interval_secs
            .max(10);
        Self::with_intervals(db, secrets, DEBOUNCE, Duration::from_secs(secs))
    }

    #[cfg(test)]
    pub fn with_debounce(
        db: Arc<Db>,
        secrets: Arc<SecretsManager>,
        debounce: Duration,
    ) -> Arc<Self> {
        Self::with_intervals(db, secrets, debounce, DEFAULT_FETCH_EVERY)
    }

    pub fn with_intervals(
        db: Arc<Db>,
        secrets: Arc<SecretsManager>,
        debounce: Duration,
        fetch_every: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            debounce,
            fetch_every,
            db,
            secrets,
            rt: Mutex::new(Runtime::default()),
            wake: tokio::sync::Notify::new(),
            cycle_lock: tokio::sync::Mutex::new(()),
            after_restore: std::sync::OnceLock::new(),
        })
    }

    /// Define o que fazer depois de uma restauração (uma vez só, no bootstrap).
    pub fn set_after_restore(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.after_restore.set(Box::new(f));
    }

    pub async fn settings(&self) -> anyhow::Result<Settings> {
        let get = |k| daemon_settings::sql_get(&self.db, k);
        let mode = get(KEY_MODE)
            .await?
            .and_then(|m| Mode::parse(&m))
            .unwrap_or(Mode::Off);
        Ok(Settings {
            mode,
            remote: get(KEY_REMOTE).await?.filter(|s| !s.is_empty()),
            branch: get(KEY_BRANCH)
                .await?
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_BRANCH.into()),
            provider_id: get(KEY_PROVIDER).await?.filter(|s| !s.is_empty()),
        })
    }

    async fn allow_set(&self) -> anyhow::Result<BTreeSet<String>> {
        Ok(daemon_settings::sql_get(&self.db, KEY_ALLOW)
            .await?
            .and_then(|j| serde_json::from_str::<BTreeSet<String>>(&j).ok())
            .unwrap_or_default())
    }

    /// As suspeitas que o usuário liberou (fingerprints), para marcar a lista do `InfraAudit`.
    pub async fn allowed(&self) -> BTreeSet<String> {
        self.allow_set().await.unwrap_or_default()
    }

    pub fn fingerprint(project: &str, scope: &str, what: &str) -> String {
        fingerprint(project, scope, what)
    }

    /// Libera (ou desfaz) uma **suspeita** da auditoria. Certezas não podem ser liberadas.
    pub async fn set_allowed(
        &self,
        project: &str,
        scope: &str,
        what: &str,
        allowed: bool,
    ) -> Result<(), StateGitError> {
        let fp = fingerprint(project, scope, what);
        let mut set = self.allow_set().await?;
        if allowed {
            let found = plaintext_findings(&self.db.infra().snapshot())
                .into_iter()
                .find(|f| fingerprint(&f.project, &f.scope, &f.what) == fp);
            match found {
                None => {
                    return Err(StateGitError::Invalid(
                        "esse item não está na auditoria".into(),
                    ));
                }
                Some(f) if f.certainty == Certainty::Definite => {
                    return Err(StateGitError::Invalid(
                        "isto é um segredo certo (senha de banco, webhook…): não pode ser liberado, só movido para secret".into(),
                    ));
                }
                Some(_) => {
                    set.insert(fp);
                }
            }
        } else {
            set.remove(&fp);
        }
        daemon_settings::set(
            &self.db,
            KEY_ALLOW,
            &serde_json::to_string(&set).map_err(anyhow::Error::from)?,
        )
        .await?;
        self.wake.notify_one();
        Ok(())
    }

    pub async fn status(&self) -> anyhow::Result<StateGitStatus> {
        let s = self.settings().await?;
        let removed = crate::store::state_git_marks::list_removed(&self.db)
            .await?
            .into_iter()
            .map(|r| StateGitRemoved {
                id: r.id,
                kind: r.kind,
                name: r.name,
                project: r.project,
                sha: r.sha,
                at: r.at,
            })
            .collect();
        let pending = crate::store::state_git_marks::list_pending(&self.db).await?;
        let rt = self.rt.lock().unwrap();
        Ok(StateGitStatus {
            mode: s.mode.as_str().into(),
            remote: s.remote,
            branch: s.branch,
            provider_id: s.provider_id,
            git_available: git::available(),
            recovery_configured: self.secrets.has_recovery(),
            last_commit: rt.last_commit.as_ref().map(|c| StateGitCommit {
                sha: c.sha.clone(),
                at: c.at.clone(),
                subject: c.subject.clone(),
            }),
            last_push_ok: rt.last_push_ok,
            last_push_at: rt.last_push_at.clone(),
            last_error: rt.last_error.clone(),
            blocked_by_plaintext: rt.blocked,
            needs_push: rt.needs_push,
            last_fetch_at: rt.last_fetch_at.clone(),
            ahead: rt.ahead,
            behind: rt.behind,
            paused_reason: rt.paused.clone(),
            incoming_rejected: rt
                .incoming_error
                .as_ref()
                .map(|(sha, msg)| StateGitRejected {
                    sha: sha.clone(),
                    reason: msg.clone(),
                }),
            removed_in_git: removed,
            pending_deploy: pending,
        })
    }

    /// Liga/desliga/configura. Ligar é **recusado** se a auditoria achar valor em claro.
    pub async fn configure(
        self: &Arc<Self>,
        mode: &str,
        remote: Option<String>,
        branch: Option<String>,
        provider_id: Option<String>,
    ) -> Result<StateGitStatus, StateGitError> {
        let mode = Mode::parse(mode).ok_or_else(|| {
            StateGitError::Invalid(format!(
                "modo desconhecido: {mode} (use off, commit ou sync)"
            ))
        })?;
        let remote = remote
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        let branch = branch
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| DEFAULT_BRANCH.into());
        let provider_id = provider_id.filter(|p| !p.is_empty());

        if mode != Mode::Off {
            if !git::available() {
                return Err(StateGitError::Invalid(
                    "o binário `git` não está instalado neste servidor".into(),
                ));
            }
            if let Some(r) = &remote {
                git::validate_remote(r).map_err(|e| StateGitError::Invalid(e.to_string()))?;
            }
            if mode == Mode::Sync {
                let r = remote.as_deref().ok_or_else(|| {
                    StateGitError::Invalid("o modo sync precisa do endereço do repositório".into())
                })?;
                if r.starts_with("https://") {
                    let pid = provider_id.as_deref().ok_or_else(|| {
                        StateGitError::Invalid(
                            "um repositório https precisa de um provider git cadastrado para a credencial".into(),
                        )
                    })?;
                    if crate::store::git_providers::get(&self.db, pid)
                        .await?
                        .is_none()
                    {
                        return Err(StateGitError::Invalid(format!(
                            "provider {pid} não encontrado"
                        )));
                    }
                }
            }
            // A trava: nada em claro pode entrar no histórico.
            let allow = self.allow_set().await?;
            let n = blocking(&self.db.infra().snapshot(), &allow).len();
            if n > 0 {
                return Err(StateGitError::Plaintext(n));
            }
            if mode == Mode::Sync {
                self.refuse_unrelated_history(&remote, &branch, &provider_id)
                    .await?;
            }
        }

        let put = |k: &'static str, v: Option<String>| {
            let db = self.db.clone();
            async move {
                match v {
                    Some(v) => daemon_settings::set(&db, k, &v).await,
                    None => daemon_settings::delete(&db, k).await,
                }
            }
        };
        put(KEY_REMOTE, remote).await?;
        put(KEY_BRANCH, Some(branch)).await?;
        put(KEY_PROVIDER, provider_id).await?;
        put(KEY_MODE, Some(mode.as_str().into())).await?;
        {
            let mut rt = self.rt.lock().unwrap();
            rt.last_error = None;
            rt.paused = None;
            rt.needs_push = mode == Mode::Sync;
        }
        if mode == Mode::Sync {
            // Servidor recém-instalado ligando o sync num repositório que já tem história: traz o
            // estado de volta (Fase 8) em vez de commitar um estado vazio e divergir.
            if let Err(e) = self.restore_if_virgin().await {
                let msg = format!("restauração a partir do git falhou: {e:#}");
                warn!("state_git: {msg}");
                self.rt.lock().unwrap().last_error = Some(msg);
            }
        }
        if mode != Mode::Off {
            // O primeiro commit já nasce limpo (a auditoria acabou de passar).
            self.cycle(true).await;
        }
        Ok(self.status().await?)
    }

    /// Recusa ligar o `sync` num servidor que **já tem estado** apontando para um repositório que
    /// **já tem história** sem relação com a dele. Sem isto o `configure` voltava "ok" e o desacordo só
    /// aparecia depois, como pausa por divergência: parecia que tinha dado certo e não tinha. Só um
    /// servidor virgem (sem projetos) restaura de um repositório com história (`restore_if_virgin`).
    /// Repositório vazio, ou com a história que este servidor já conhece (trocar o token ou a
    /// branch de um sync que já funcionava), passa.
    async fn refuse_unrelated_history(
        &self,
        remote: &Option<String>,
        branch: &str,
        provider_id: &Option<String>,
    ) -> Result<(), StateGitError> {
        if self.db.infra().snapshot().projects.is_empty() {
            return Ok(());
        }
        let s = Settings {
            mode: Mode::Sync,
            remote: remote.clone(),
            branch: branch.to_string(),
            provider_id: provider_id.clone(),
        };
        let auth = self.auth_for(&s).await?;
        let (db, b, r) = (self.db.clone(), branch.to_string(), remote.clone());
        let unrelated = tokio::task::spawn_blocking(move || {
            db.infra().exclusive(|root, _state| {
                git::ensure_repo(root, &b, r.as_deref())?;
                if !git::fetch(root, &b, auth.as_ref())? || git::remote_head(root, &b).is_none() {
                    return anyhow::Ok(false); // repositório vazio
                }
                anyhow::Ok(!git::shares_history(root, &b))
            })
        })
        .await
        .map_err(anyhow::Error::from)??;
        if unrelated {
            return Err(StateGitError::Invalid(format!(
                "o repositório já tem histórico na branch `{branch}` e este servidor já tem projetos: \
                 os dois não têm origem em comum e nada seria restaurado. Para restaurar, use um \
                 servidor limpo (sem projetos); para começar do zero, aponte para um repositório \
                 vazio ou outra branch."
            )));
        }
        Ok(())
    }

    /// **Restauração num servidor limpo (Fase 8).** Só age quando o servidor é virgem: nenhum projeto
    /// e nenhum commit local. Busca o remoto e, se a branch existe, aplica a árvore dela (com todas
    /// as validações do pull, inclusive a de que os secrets decifram com a identidade deste
    /// servidor: para restaurar secrets, o `age_identity` precisa ser a chave de recuperação ou a
    /// do servidor antigo). Um servidor que já tem estado próprio nunca é sobrescrito: segue o
    /// caminho normal e, se os históricos divergirem, pausa.
    async fn restore_if_virgin(&self) -> anyhow::Result<()> {
        if !self.db.infra().snapshot().projects.is_empty() {
            return Ok(());
        }
        let s = self.settings().await?;
        let auth = self.auth_for(&s).await?;
        let (db, branch, remote) = (self.db.clone(), s.branch.clone(), s.remote.clone());
        let sha = tokio::task::spawn_blocking(move || {
            db.infra().exclusive(|root, state| {
                if !state.projects.is_empty() {
                    return Ok(None);
                }
                git::ensure_repo(root, &branch, remote.as_deref())?;
                if git::head(root)?.is_some() {
                    return Ok(None);
                }
                if !git::fetch(root, &branch, auth.as_ref())? {
                    return Ok(None); // repositório vazio: nada a restaurar
                }
                anyhow::Ok(git::remote_head(root, &branch))
            })
        })
        .await??;
        if let Some(sha) = sha {
            info!(sha = %sha, "state_git: servidor limpo, restaurando o estado do repositório");
            if let Applied::No(msg) = self.apply_incoming(&sha, true).await {
                anyhow::bail!(msg);
            }
            // O servidor é novo: o que veio do git é para estar no ar. Marca a intenção e religa
            // na ordem das dependências (as imagens são baixadas/construídas; dados não voltam).
            let services = crate::store::services::list_all(&self.db).await?;
            for svc in services.iter().filter(|s| s.spec.autostart) {
                crate::store::services::set_desired_state(
                    &self.db,
                    &svc.id,
                    crate::store::services::DesiredState::Running,
                )
                .await?;
            }
            if let Some(f) = self.after_restore.get() {
                f();
            }
        }
        Ok(())
    }

    /// "Sincronizar agora": commit pendente e, em `sync`, push.
    pub async fn sync_now(&self) -> Result<StateGitStatus, StateGitError> {
        if self.settings().await?.mode == Mode::Off {
            return Err(StateGitError::Invalid(
                "a sincronização com o git está desligada".into(),
            ));
        }
        self.cycle(true).await;
        Ok(self.status().await?)
    }

    /// Um ciclo: audita, commita (se houver diff) e, em `sync`, envia. **Nunca falha para fora**:
    /// o resultado vai para o status, e o daemon segue.
    pub async fn cycle(&self, force_push: bool) {
        let _one_at_a_time = self.cycle_lock.lock().await;
        if let Err(e) = self.cycle_inner(force_push).await {
            let msg = format!("{e:#}");
            warn!(error = %msg, "state_git: ciclo falhou");
            self.rt.lock().unwrap().last_error = Some(msg);
        }
    }

    /// Commit do que há de local: dentro do lock de escrita, para o `git` só ver o diretório entre
    /// transações e para a auditoria valer para exatamente o que será commitado. `Ok(true)` =
    /// **suspenso** por valor em claro (nada foi commitado).
    async fn commit_local(&self, s: &Settings) -> anyhow::Result<bool> {
        let allow = self.allow_set().await?;
        enum Step {
            Blocked(usize),
            Done {
                committed: bool,
                head: Option<CommitInfo>,
            },
        }
        let db = self.db.clone();
        let (branch, remote) = (s.branch.clone(), s.remote.clone());
        let step = tokio::task::spawn_blocking(move || -> anyhow::Result<Step> {
            db.infra().exclusive(|root, state| {
                let n = blocking(state, &allow).len();
                if n > 0 {
                    return Ok(Step::Blocked(n));
                }
                git::ensure_repo(root, &branch, remote.as_deref())?;
                let c = git::commit_all(root, |paths| subject(paths, state))?;
                Ok(Step::Done {
                    committed: c.is_some(),
                    head: git::head(root)?,
                })
            })
        })
        .await??;

        match step {
            Step::Blocked(n) => {
                let mut rt = self.rt.lock().unwrap();
                rt.blocked = n;
                rt.last_error = Some(format!(
                    "commit suspenso: {n} valor(es) em claro no estado (veja InfraAudit)"
                ));
                Ok(true)
            }
            Step::Done { committed, head } => {
                let mut rt = self.rt.lock().unwrap();
                rt.blocked = 0;
                if rt
                    .last_error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("commit suspenso"))
                {
                    rt.last_error = None;
                }
                if let Some(h) = head {
                    if committed {
                        info!(sha = %h.sha, subject = %h.subject, "state_git: commit");
                    }
                    rt.last_commit = Some(h);
                }
                if committed {
                    rt.needs_push = true;
                }
                Ok(false)
            }
        }
    }

    async fn cycle_inner(&self, force_push: bool) -> anyhow::Result<()> {
        let s = self.settings().await?;
        if s.mode == Mode::Off {
            return Ok(());
        }
        // Pausada (histórico divergiu): o daemon **não mexe no git** — nem commita. Quem está
        // resolvendo à mão (um `merge` por SSH) não pode ter o `git add -A` do daemon capturando um
        // merge pela metade. As edições feitas pela interface continuam valendo (estão nos arquivos)
        // e são commitadas quando a sincronização é retomada.
        if self.rt.lock().unwrap().paused.is_some() {
            return Ok(());
        }
        if self.commit_local(&s).await? {
            return Ok(()); // suspenso: valor em claro
        }
        if s.mode != Mode::Sync {
            return Ok(());
        }

        // ── Fase 7: trazer o que mudou por fora ─────────────────────────────────────────────────
        match self.reconcile(&s).await {
            Ok(Reconciled::Push) => {}
            Ok(Reconciled::Done) => return Ok(()),
            Err(e) => {
                let msg = format!("fetch falhou: {e:#}");
                warn!(error = %msg, "state_git");
                self.rt.lock().unwrap().last_error = Some(msg);
                return Ok(());
            }
        }
        let push_wanted = {
            let rt = self.rt.lock().unwrap();
            rt.needs_push || force_push
        };
        if push_wanted {
            self.push(&s).await;
        }
        Ok(())
    }

    /// `fetch` + comparação dos históricos. Decide entre: nada a fazer / enviar o que é nosso /
    /// aplicar o que veio de fora (só se o local não tem nada novo) / **pausar** (divergiu).
    async fn reconcile(&self, s: &Settings) -> anyhow::Result<Reconciled> {
        let auth = self.auth_for(s).await?;
        let (db, branch) = (self.db.clone(), s.branch.clone());
        let (has_remote_branch, remote, counts) = tokio::task::spawn_blocking(move || {
            // o fetch só mexe em objetos e refs; não precisa do lock de escrita
            let root = db.infra().tree().root().to_path_buf();
            let present = git::fetch(&root, &branch, auth.as_ref())?;
            let remote = git::remote_head(&root, &branch);
            let counts = match (&remote, git::head(&root)?) {
                (Some(_), Some(_)) => Some(git::ahead_behind(&root, &branch)?),
                _ => None,
            };
            anyhow::Ok((present, remote, counts))
        })
        .await??;
        {
            let mut rt = self.rt.lock().unwrap();
            rt.last_fetch_at = Some(chrono::Utc::now().to_rfc3339());
            // o remoto que sumiu/ainda não existe não é erro: o push cria a branch
            if !has_remote_branch {
                rt.ahead = 0;
                rt.behind = 0;
                return Ok(Reconciled::Push);
            }
        }
        let (Some(remote_sha), Some((ahead, behind))) = (remote, counts) else {
            return Ok(Reconciled::Push);
        };
        {
            let mut rt = self.rt.lock().unwrap();
            rt.ahead = ahead;
            rt.behind = behind;
            rt.needs_push = ahead > 0;
            // Nada vindo do remoto: não há commit recusado a mostrar. (O remoto pode ter sido
            // corrigido, ou reescrito de volta ao que o servidor já tem.)
            if behind == 0
                && let Some((_, msg)) = rt.incoming_error.take()
                && rt.last_error.as_deref() == Some(msg.as_str())
            {
                rt.last_error = None;
            }
        }
        match (ahead > 0, behind > 0) {
            (false, false) => Ok(Reconciled::Done),
            (true, false) => Ok(Reconciled::Push),
            (true, true) => {
                let msg = format!(
                    "o histórico divergiu: o servidor tem {ahead} commit(s) que o remoto não tem e o remoto tem {behind} que o servidor não tem. \
                     A sincronização foi pausada (o estado local segue funcionando). Resolva no repositório e use \"retomar\"."
                );
                warn!("state_git: {msg}");
                let mut rt = self.rt.lock().unwrap();
                rt.paused = Some(msg.clone());
                rt.last_error = Some(msg);
                Ok(Reconciled::Done)
            }
            (false, true) => {
                self.apply_incoming(&remote_sha, false).await;
                Ok(Reconciled::Done)
            }
        }
    }

    /// Aplica o que chegou por fora: lê a árvore do commit **sem checkout**, valida tudo e grava
    /// como uma transação. Qualquer recusa deixa o estado como está e fica visível no status.
    async fn apply_incoming(&self, sha: &str, force: bool) -> Applied {
        let short: String = sha.chars().take(7).collect();
        let allow = self.allow_set().await.unwrap_or_default();
        let (db, secrets, rev) = (self.db.clone(), self.secrets.clone(), sha.to_string());
        let result = tokio::task::spawn_blocking(move || {
            let root = db.infra().tree().root().to_path_buf();
            let files = git::files_at(&root, &rev, crate::store::infra::model::is_state_file)?;
            let rev2 = rev.clone();
            db.infra().apply_files(
                &files,
                |root, _old, new| validate_incoming(root, new, &allow, &secrets, !force),
                move |root| {
                    git::reset_hard(root, &rev2)
                        .map_err(|e| anyhow::Error::new(GitStuck(format!("{e:#}"))))
                },
            )
        })
        .await;
        let result = match result {
            Ok(r) => r,
            Err(e) => Err(anyhow::Error::from(e)),
        };
        match result {
            Ok((old, new)) => {
                info!(sha = %short, "state_git: estado aplicado a partir do git");
                {
                    let mut rt = self.rt.lock().unwrap();
                    rt.incoming_error = None;
                    rt.last_error = None;
                    rt.ahead = 0;
                    rt.behind = 0;
                    rt.needs_push = false;
                }
                if let Err(e) = self.record_marks(sha, &old, &new).await {
                    warn!(error = %e, "state_git: não consegui registrar as marcas do pull");
                }
                let db = self.db.clone();
                if let Ok(Ok(Some(h))) =
                    tokio::task::spawn_blocking(move || git::head(db.infra().tree().root())).await
                {
                    self.rt.lock().unwrap().last_commit = Some(h);
                }
                Applied::Yes
            }
            Err(e) if e.downcast_ref::<RetryLater>().is_some() => {
                info!(sha = %short, "state_git: pull adiado ({e})");
                Applied::Later
            }
            Err(e) => {
                let stuck = e.downcast_ref::<GitStuck>().is_some();
                let msg = if stuck {
                    format!("o estado de {short} foi aplicado, mas o git não avançou: {e:#}")
                } else {
                    format!("o commit {short} do remoto foi recusado e NÃO foi aplicado: {e:#}")
                };
                warn!("state_git: {msg}");
                let mut rt = self.rt.lock().unwrap();
                rt.incoming_error = Some((sha.to_string(), msg.clone()));
                rt.last_error = Some(msg.clone());
                Applied::No(msg)
            }
        }
    }

    /// Registra o que o `pull` deixou para decisão humana: o que **sumiu** (nunca apagamos container
    /// nem dado por isso) e os serviços cujo declarado mudou (o que roda só muda no próximo deploy).
    async fn record_marks(
        &self,
        sha: &str,
        old: &InfraState,
        new: &InfraState,
    ) -> anyhow::Result<()> {
        use crate::store::state_git_marks as marks;
        let d = diff_states(old, new);
        let at = chrono::Utc::now().to_rfc3339();
        for r in d.removed {
            marks::add_removed(
                &self.db,
                &marks::Removed {
                    id: r.id,
                    kind: r.kind,
                    name: r.name,
                    project: r.project,
                    sha: sha.to_string(),
                    at: at.clone(),
                    compose_project: r.compose_project,
                },
            )
            .await?;
        }
        for id in d.spec_changed {
            marks::set_pending(&self.db, &id, sha).await?;
        }
        // Rollback no git: o que voltou ao estado deixa de estar "removido" (o container nunca foi
        // tocado e é reconhecido pelo mesmo id / nome de stack).
        let services: std::collections::BTreeSet<String> = new
            .projects
            .iter()
            .flat_map(|p| p.services.iter().map(|s| s.id.clone()))
            .collect();
        let projects: std::collections::BTreeSet<String> =
            new.projects.iter().map(|p| p.id.clone()).collect();
        marks::clear_returned(&self.db, &services, &projects).await?;
        Ok(())
    }

    /// Retomar depois de resolver o histórico no repositório: tira a pausa e reavalia.
    pub async fn resume(&self) -> Result<StateGitStatus, StateGitError> {
        if self.settings().await?.mode == Mode::Off {
            return Err(StateGitError::Invalid(
                "a sincronização com o git está desligada".into(),
            ));
        }
        // Alguém pode ter resolvido à mão (merge por SSH): relê a pasta e valida ANTES de voltar a
        // andar, senão a próxima escrita sobrescreveria o que a pessoa resolveu. Pasta inválida =
        // erro claro e continua pausado.
        let db = self.db.clone();
        let (old, new) = tokio::task::spawn_blocking(move || db.infra().reload_checked())
            .await
            .map_err(anyhow::Error::from)?
            .map_err(|e| {
                StateGitError::Invalid(format!(
                    "a pasta infra/ não está num estado válido (resolveu o merge? sobrou marcador de conflito?): {e:#}"
                ))
            })?;
        let db = self.db.clone();
        let head = tokio::task::spawn_blocking(move || git::head(db.infra().tree().root()))
            .await
            .map_err(anyhow::Error::from)??;
        if let Some(h) = &head
            && let Err(e) = self.record_marks(&h.sha, &old, &new).await
        {
            warn!(error = %e, "state_git: não consegui registrar as marcas ao retomar");
        }
        {
            let mut rt = self.rt.lock().unwrap();
            rt.paused = None;
            rt.incoming_error = None;
            rt.last_error = None;
        }
        self.cycle(true).await;
        Ok(self.status().await?)
    }

    /// Resolve um histórico divergido **sem terminal**, descartando um dos lados — com cópia de
    /// segurança do que for descartado:
    /// - `UseRemote`: o servidor adota o repositório; os commits locais ficam na branch
    ///   `rustploy-backup/local-<data>` do clone do servidor. O conteúdo do remoto passa antes por
    ///   todas as validações de um pull;
    /// - `UseLocal`: o repositório adota o servidor; o que o remoto tinha é copiado para
    ///   `rustploy-backup/remote-<data>` **no próprio remoto** antes de ser sobrescrito
    ///   (`--force-with-lease`: se alguém empurrar nesse meio-tempo, recusa).
    pub async fn resolve(
        self: &Arc<Self>,
        choice: shared::StateGitResolveChoice,
    ) -> Result<StateGitStatus, StateGitError> {
        use shared::StateGitResolveChoice::*;
        let s = self.settings().await?;
        if s.mode != Mode::Sync {
            return Err(StateGitError::Invalid(
                "só há divergência a resolver no modo sync".into(),
            ));
        }
        if self.rt.lock().unwrap().paused.is_none() {
            return Err(StateGitError::Invalid(
                "a sincronização não está pausada: não há divergência a resolver".into(),
            ));
        }
        let one_at_a_time = self.cycle_lock.lock().await;

        // 1. O que existe de local precisa estar commitado (é isso que a cópia de segurança guarda).
        if self.commit_local(&s).await? {
            return Err(StateGitError::Invalid(
                "há valores em claro ainda não commitados no servidor (veja InfraAudit): resolva-os antes de resolver a divergência".into(),
            ));
        }
        // 2. O remoto como está agora.
        let auth = self.auth_for(&s).await?;
        let (db, branch) = (self.db.clone(), s.branch.clone());
        let a2 = auth.clone();
        let remote_sha = tokio::task::spawn_blocking(move || {
            let root = db.infra().tree().root().to_path_buf();
            git::fetch(&root, &branch, a2.as_ref())?;
            anyhow::Ok(git::remote_head(&root, &branch))
        })
        .await
        .map_err(anyhow::Error::from)??
        .ok_or_else(|| StateGitError::Invalid("o remoto não tem a branch configurada".into()))?;

        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
        match choice {
            UseRemote => {
                let (db, name) = (self.db.clone(), format!("rustploy-backup/local-{stamp}"));
                tokio::task::spawn_blocking(move || {
                    git::backup_branch(db.infra().tree().root(), &name, "HEAD")
                })
                .await
                .map_err(anyhow::Error::from)??;
                match self.apply_incoming(&remote_sha, true).await {
                    Applied::Yes => {}
                    Applied::Later => {
                        return Err(StateGitError::Invalid("tente de novo em instantes".into()));
                    }
                    Applied::No(m) => {
                        return Err(StateGitError::Invalid(format!(
                            "o conteúdo do remoto não pôde ser adotado: {m}"
                        )));
                    }
                }
            }
            UseLocal => {
                let (db, branch, sha) = (self.db.clone(), s.branch.clone(), remote_sha.clone());
                let name = format!("rustploy-backup/remote-{stamp}");
                tokio::task::spawn_blocking(move || {
                    let root = db.infra().tree().root().to_path_buf();
                    git::push_backup_ref(&root, &sha, &name, auth.as_ref())?;
                    git::push_force_with_lease(&root, &branch, &sha, auth.as_ref())
                })
                .await
                .map_err(anyhow::Error::from)??;
            }
        }
        info!(?choice, "state_git: divergência resolvida");
        {
            let mut rt = self.rt.lock().unwrap();
            rt.paused = None;
            rt.incoming_error = None;
            rt.last_error = None;
        }
        drop(one_at_a_time);
        self.cycle(true).await;
        Ok(self.status().await?)
    }

    /// Esquece uma marca de "removido no git" (a pessoa decidiu manter o que ainda roda, ou já limpou).
    pub async fn dismiss_removal(&self, id: &str) -> Result<(), StateGitError> {
        if !crate::store::state_git_marks::dismiss_removed(&self.db, id).await? {
            return Err(StateGitError::Invalid("essa marca não existe".into()));
        }
        Ok(())
    }

    /// Resolve a credencial do remoto: `https://` usa o token do provider; `file://` não usa nada.
    async fn auth_for(&self, s: &Settings) -> anyhow::Result<Option<Auth>> {
        let Some(remote) = &s.remote else {
            anyhow::bail!("sem repositório configurado");
        };
        if !remote.starts_with("https://") {
            return Ok(None);
        }
        let pid = s
            .provider_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("sem provider git configurado para o remoto https"))?;
        let p = crate::store::git_providers::get(&self.db, pid)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider {pid} não encontrado"))?;
        let token = git_providers::usable_token(&self.secrets, &p)?;
        Ok(Some(Auth {
            username: p
                .account_login
                .clone()
                .unwrap_or_else(|| "x-access-token".into()),
            token,
        }))
    }

    async fn push(&self, s: &Settings) {
        let attempt = |auth: Option<Auth>| {
            let (db, branch) = (self.db.clone(), s.branch.clone());
            async move {
                tokio::task::spawn_blocking(move || {
                    db.infra()
                        .exclusive(|root, _| git::push(root, &branch, auth.as_ref()))
                })
                .await
                .map_err(anyhow::Error::from)?
            }
        };
        let mut result = match self.auth_for(s).await {
            Ok(auth) => attempt(auth).await,
            Err(e) => Err(e),
        };
        // Token expirado: renova uma vez e tenta de novo.
        if let Err(e) = &result {
            let m = e.to_string();
            let denied = ["401", "403", "Authentication failed", "invalid credentials"]
                .iter()
                .any(|p| m.contains(p));
            if denied
                && let Some(pid) = &s.provider_id
                && let Ok(Some(p)) = crate::store::git_providers::get(&self.db, pid).await
                && git_providers::refresh_access_token(&self.db, &self.secrets, &p)
                    .await
                    .is_some()
                && let Ok(auth) = self.auth_for(s).await
            {
                result = attempt(auth).await;
            }
        }
        let now = chrono::Utc::now().to_rfc3339();
        let mut rt = self.rt.lock().unwrap();
        rt.last_push_at = Some(now);
        match result {
            Ok(()) => {
                info!("state_git: push ok");
                rt.last_push_ok = Some(true);
                rt.needs_push = false;
                rt.last_error = None;
            }
            Err(e) => {
                let msg = format!("push falhou: {e:#}");
                warn!(error = %msg, "state_git");
                rt.last_push_ok = Some(false);
                rt.last_error = Some(msg);
            }
        }
    }

    /// O worker: acorda a cada mudança do estado (com debounce), a cada "sincronizar agora" e
    /// periodicamente para repetir o que falhou. Só faz algo se o modo não for `off`.
    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut changes = me.db.infra().subscribe();
            changes.mark_unchanged();
            // No boot: commita o que mudou enquanto o daemon estava parado e reenvia o pendente.
            me.rt.lock().unwrap().needs_push = true;
            me.cycle(false).await;
            let mut tick = tokio::time::interval(me.fetch_every);
            tick.tick().await;
            loop {
                tokio::select! {
                    r = changes.changed() => {
                        if r.is_err() { return; }
                        // várias edições seguidas viram um commit só
                        loop {
                            tokio::select! {
                                r = changes.changed() => { if r.is_err() { return; } }
                                _ = tokio::time::sleep(me.debounce) => break,
                            }
                        }
                    }
                    _ = me.wake.notified() => {}
                    _ = tick.tick() => {}
                }
                me.cycle(false).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{EnvVar, EnvVarValue};

    struct Cenario {
        db: Arc<Db>,
        sg: Arc<StateGit>,
        secrets: Arc<SecretsManager>,
        _dir: tempfile::TempDir,
    }

    async fn cenario() -> Cenario {
        cenario_com(true).await
    }

    /// Um servidor recém-instalado: sem nenhum projeto.
    async fn cenario_limpo() -> Cenario {
        cenario_com(false).await
    }

    async fn cenario_com(com_projeto: bool) -> Cenario {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::store::connect(&dir.path().join("data"))
                .await
                .unwrap(),
        );
        let secrets = Arc::new(SecretsManager::new(db.clone()).unwrap());
        if com_projeto {
            db.seed_project_for_test("p1");
        }
        let sg = StateGit::with_debounce(db.clone(), secrets.clone(), Duration::from_millis(150));
        Cenario {
            db,
            sg,
            secrets,
            _dir: dir,
        }
    }

    fn infra_dir(c: &Cenario) -> std::path::PathBuf {
        c.db.infra().exclusive(|root, _| root.to_path_buf())
    }

    fn git_out(c: &Cenario, args: &[&str]) -> String {
        git::run(&infra_dir(c), args, None).unwrap()
    }

    fn add_env(c: &Cenario, key: &str, value: &str) {
        c.db.infra()
            .update(|st| {
                st.projects[0].env_vars.push(EnvVar {
                    key: key.into(),
                    value: EnvVarValue::Plain(value.into()),
                });
                Ok(())
            })
            .unwrap();
    }

    fn bare_remote() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        git::run(d.path(), &["init", "-q", "--bare"], None).unwrap();
        let url = format!("file://{}", d.path().display());
        (d, url)
    }

    #[tokio::test]
    async fn desligado_por_padrao_e_nao_cria_repositorio() {
        let c = cenario().await;
        let st = c.sg.status().await.unwrap();
        assert_eq!((st.mode.as_str(), st.git_available), ("off", true));
        c.sg.cycle(true).await;
        assert!(!infra_dir(&c).join(".git").exists());
        assert!(c.sg.sync_now().await.is_err());
    }

    #[tokio::test]
    async fn ligar_com_valor_em_claro_e_recusado_e_nada_e_criado() {
        let c = cenario().await;
        add_env(&c, "STRIPE_SECRET_KEY", "sk_live_123");
        let e =
            c.sg.configure("commit", None, None, None)
                .await
                .unwrap_err();
        assert!(matches!(e, StateGitError::Plaintext(1)), "{e}");
        assert_eq!(c.sg.status().await.unwrap().mode, "off");
        assert!(
            !infra_dir(&c).join(".git").exists(),
            "nem o repositório pode nascer"
        );
        // a mensagem não carrega o valor
        assert!(!e.to_string().contains("sk_live_123"));
    }

    #[tokio::test]
    async fn modo_invalido_remoto_invalido_e_sync_sem_remoto() {
        let c = cenario().await;
        for (mode, remote) in [
            ("talvez", None),
            ("commit", Some("ssh://git@x/y.git")),
            (
                "commit",
                Some("https://user:pw-super-secreto@github.com/o/r.git"),
            ),
            ("sync", None),
            ("sync", Some("https://github.com/o/r.git")), // https sem provider
        ] {
            let e =
                c.sg.configure(mode, remote.map(String::from), None, None)
                    .await
                    .unwrap_err();
            assert!(
                matches!(e, StateGitError::Invalid(_)),
                "{mode} {remote:?}: {e}"
            );
            assert!(!e.to_string().contains("pw-super-secreto"), "{e}");
        }
        assert_eq!(c.sg.status().await.unwrap().mode, "off");
    }

    #[tokio::test]
    async fn ligar_limpo_commita_o_inicial_e_depois_so_o_que_muda() {
        let c = cenario().await;
        let st =
            c.sg.configure("commit", None, Some("estado".into()), None)
                .await
                .unwrap();
        let primeiro = st.last_commit.expect("commit inicial");
        assert_eq!(
            git_out(&c, &["symbolic-ref", "--short", "HEAD"]).trim(),
            "estado"
        );
        let tracked = git_out(&c, &["ls-files"]);
        assert!(
            tracked.contains("MANIFEST.json") && tracked.contains("projects/p1/project.yml"),
            "{tracked}"
        );
        assert!(
            !tracked.contains(".lock") && !tracked.contains(".staging"),
            "{tracked}"
        );

        // sem mudança, sem commit novo
        c.sg.cycle(false).await;
        assert_eq!(
            c.sg.status().await.unwrap().last_commit.unwrap().sha,
            primeiro.sha
        );

        // uma mudança vira um commit com o nome do projeto no assunto
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("novo".into());
                Ok(())
            })
            .unwrap();
        c.sg.cycle(false).await;
        let depois = c.sg.status().await.unwrap().last_commit.unwrap();
        assert_ne!(depois.sha, primeiro.sha);
        assert!(depois.subject.contains("projeto p1"), "{}", depois.subject);
        assert_eq!(git_out(&c, &["rev-list", "--count", "HEAD"]).trim(), "2");
        // commit local não fala com a rede: nada a enviar em modo commit
        assert_eq!(c.sg.status().await.unwrap().last_push_ok, None);
    }

    #[tokio::test]
    async fn o_historico_nunca_contem_segredo_em_claro() {
        let c = cenario().await;
        c.sg.configure("commit", None, None, None).await.unwrap();
        let inicial = c.sg.status().await.unwrap().last_commit.unwrap().sha;

        // alguém (ou o "mover para env") põe um segredo em claro: o commit é suspenso
        add_env(&c, "API_TOKEN", "valor-ultra-secreto");
        c.sg.cycle(false).await;
        let st = c.sg.status().await.unwrap();
        assert_eq!(st.blocked_by_plaintext, 1);
        assert_eq!(
            st.last_commit.unwrap().sha,
            inicial,
            "nada novo foi commitado"
        );
        assert!(st.last_error.unwrap().contains("commit suspenso"));

        // corrigir: virar secret → destrava sozinho, e o que entra no git é só o limpo
        c.secrets
            .move_secret("p1", None, "API_TOKEN", shared::SecretMoveOp::EnvToSecret)
            .await
            .unwrap();
        c.sg.cycle(false).await;
        let st = c.sg.status().await.unwrap();
        assert_eq!((st.blocked_by_plaintext, st.last_error), (0, None));
        assert_ne!(st.last_commit.unwrap().sha, inicial);

        let tudo = git_out(&c, &["log", "-p", "--all"]);
        assert!(
            !tudo.contains("valor-ultra-secreto"),
            "segredo vazou para o histórico"
        );
        assert!(tudo.contains("ENC[age:"), "o secret entra cifrado");
    }

    #[tokio::test]
    async fn suspeita_liberada_destrava_mas_certeza_nao_pode_ser_liberada() {
        let c = cenario().await;
        add_env(&c, "PASSWORD_MIN_LENGTH", "8"); // falso positivo clássico
        assert!(matches!(
            c.sg.configure("commit", None, None, None)
                .await
                .unwrap_err(),
            StateGitError::Plaintext(1)
        ));
        c.sg.set_allowed("App", "projeto", "env PASSWORD_MIN_LENGTH", true)
            .await
            .expect_err("o projeto se chama p1, não App");
        c.sg.set_allowed("p1", "projeto", "env PASSWORD_MIN_LENGTH", true)
            .await
            .unwrap();
        c.sg.configure("commit", None, None, None).await.unwrap();

        // uma certeza (webhook sem cifra) não pode ser liberada
        c.db.seed_service_for_test("p1", "s1");
        c.db.infra()
            .update(|st| {
                st.projects[0].services[0].webhook_token = Some("em-claro".into());
                Ok(())
            })
            .unwrap();
        let e =
            c.sg.set_allowed("p1", "serviço s1", "token de webhook sem cifra", true)
                .await
                .unwrap_err();
        assert!(e.to_string().contains("não pode ser liberado"), "{e}");
        // desfazer a liberação volta a travar
        c.sg.set_allowed("p1", "projeto", "env PASSWORD_MIN_LENGTH", false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sync_envia_para_o_remoto_e_um_push_que_falha_nao_derruba() {
        let c = cenario().await;
        let (remote_dir, url) = bare_remote();
        let st =
            c.sg.configure("sync", Some(url.clone()), None, None)
                .await
                .unwrap();
        assert_eq!(
            (st.last_push_ok, st.needs_push),
            (Some(true), false),
            "{:?}",
            st.last_error
        );
        let no_remoto = git::run(
            remote_dir.path(),
            &["log", "-1", "--format=%s", "main"],
            None,
        )
        .unwrap();
        assert!(no_remoto.starts_with("Atualiza"), "{no_remoto}");

        // mudança + "sincronizar agora" → o remoto acompanha
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("v2".into());
                Ok(())
            })
            .unwrap();
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.last_push_ok, Some(true));
        let local = git_out(&c, &["rev-parse", "HEAD"]);
        let remoto = git::run(remote_dir.path(), &["rev-parse", "main"], None).unwrap();
        assert_eq!(local.trim(), remoto.trim());

        // o remoto some: o push falha, o commit local fica, o status mostra o erro e nada quebra
        drop(remote_dir);
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("v3".into());
                Ok(())
            })
            .unwrap();
        let st = c.sg.sync_now().await.unwrap();
        // com o remoto fora do ar o `fetch` já falha (antes de tentar o push); nada quebra
        assert!(st.needs_push, "o commit local fica pendente de envio");
        assert!(st.last_error.unwrap().contains("fetch falhou"));
        assert!(
            git_out(&c, &["log", "-1", "--format=%s"])
                .trim()
                .starts_with("Atualiza")
        );
        // e o estado em memória segue funcionando
        assert!(crate::store::projects::list(&c.db).await.unwrap().len() == 1);
    }

    #[tokio::test]
    async fn desligar_para_de_commitar_e_preserva_o_historico() {
        let c = cenario().await;
        c.sg.configure("commit", None, None, None).await.unwrap();
        let n = git_out(&c, &["rev-list", "--count", "HEAD"]);
        c.sg.configure("off", None, None, None).await.unwrap();
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("depois de desligar".into());
                Ok(())
            })
            .unwrap();
        c.sg.cycle(true).await;
        assert_eq!(git_out(&c, &["rev-list", "--count", "HEAD"]), n);
    }

    #[tokio::test]
    async fn o_worker_junta_varias_edicoes_seguidas_num_commit_so() {
        let c = cenario().await;
        c.sg.configure("commit", None, None, None).await.unwrap();
        let base: u32 = git_out(&c, &["rev-list", "--count", "HEAD"])
            .trim()
            .parse()
            .unwrap();
        c.sg.spawn();
        tokio::time::sleep(Duration::from_millis(80)).await;
        for i in 0..4 {
            c.db.infra()
                .update(|st| {
                    st.projects[0].description = Some(format!("edição {i}"));
                    Ok(())
                })
                .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        let n: u32 = git_out(&c, &["rev-list", "--count", "HEAD"])
            .trim()
            .parse()
            .unwrap();
        assert_eq!(n, base + 1, "4 edições dentro do debounce = 1 commit");
        assert!(git_out(&c, &["show", "HEAD:projects/p1/project.yml"]).contains("edição 3"));
    }

    // ── Fase 7: o que chega por fora ────────────────────────────────────────────────────────────

    use crate::store::infra::model::ProjectInfra;
    use crate::store::infra::yaml::{from_yaml, to_yaml};

    /// Um "colega": clone próprio do remoto, que edita à mão e dá push.
    struct Externo {
        dir: tempfile::TempDir,
    }

    impl Externo {
        fn clone_de(url: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            // O remoto `--bare` nasce com HEAD em `master`; a nossa branch é `main`.
            if git::run(dir.path(), &["clone", "-q", "-b", "main", url, "."], None).is_err() {
                // remoto ainda vazio: clona assim mesmo e começa a branch `main` aqui
                git::run(dir.path(), &["clone", "-q", url, "."], None).unwrap();
                git::run(dir.path(), &["checkout", "-q", "-B", "main"], None).unwrap();
            }
            Externo { dir }
        }
        fn path(&self, rel: &str) -> std::path::PathBuf {
            self.dir.path().join(rel)
        }
        /// Lê `projects/<id>/project.yml`, deixa `f` mexer e grava **reserializado** (formatação
        /// diferente da do daemon, como numa edição à mão).
        fn edita_projeto(&self, id: &str, f: impl FnOnce(&mut ProjectInfra)) {
            let p = self.path(&format!("projects/{id}/project.yml"));
            let mut proj: ProjectInfra = from_yaml(&std::fs::read(&p).unwrap()).unwrap();
            f(&mut proj);
            std::fs::write(&p, to_yaml(&proj).unwrap()).unwrap();
        }
        fn commit_push(&self, msg: &str) -> String {
            git::run(self.dir.path(), &["add", "-A"], None).unwrap();
            git::run(self.dir.path(), &["commit", "-q", "-m", msg], None).unwrap();
            git::run(
                self.dir.path(),
                &["push", "-q", "origin", "HEAD:refs/heads/main"],
                None,
            )
            .unwrap();
            git::run(self.dir.path(), &["rev-parse", "HEAD"], None)
                .unwrap()
                .trim()
                .to_string()
        }
    }

    /// Cenário em modo `sync` com um remoto local já sincronizado.
    async fn com_remoto() -> (Cenario, tempfile::TempDir, String) {
        let c = cenario().await;
        c.db.seed_service_for_test("p1", "s1");
        let (remote, url) = bare_remote();
        let st =
            c.sg.configure("sync", Some(url.clone()), None, None)
                .await
                .unwrap();
        assert_eq!(st.last_push_ok, Some(true), "{:?}", st.last_error);
        (c, remote, url)
    }

    fn head_local(c: &Cenario) -> String {
        git_out(c, &["rev-parse", "HEAD"]).trim().to_string()
    }

    #[tokio::test]
    async fn edicao_externa_valida_e_aplicada_byte_a_byte_e_sem_laco() {
        let (c, remote, url) = com_remoto().await;
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| p.description = Some("editado por fora".into()));
        // um comentário que só sobrevive se o daemon gravar os bytes do commit, não reserializar
        let f = ext.path("projects/p1/project.yml");
        let txt = std::fs::read_to_string(&f).unwrap();
        std::fs::write(&f, format!("# editado à mão no GitHub\n{txt}")).unwrap();
        let sha = ext.commit_push("edita descrição");

        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.last_error, None, "{st:?}");
        assert_eq!(
            crate::store::projects::get(&c.db, "p1")
                .await
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("editado por fora")
        );
        assert_eq!(head_local(&c), sha, "o HEAD andou até o commit externo");
        let em_disco =
            std::fs::read_to_string(infra_dir(&c).join("projects/p1/project.yml")).unwrap();
        assert!(
            em_disco.starts_with("# editado à mão no GitHub"),
            "bytes do commit preservados"
        );
        assert_eq!((st.ahead, st.behind), (0, 0));

        // sem laço: aplicar não gera commit de volta nem push
        let antes = head_local(&c);
        c.sg.cycle(false).await;
        c.sg.cycle(true).await;
        assert_eq!(head_local(&c), antes);
        let st = c.sg.status().await.unwrap();
        assert!(!st.needs_push);
        assert_eq!(
            git::run(remote.path(), &["rev-parse", "main"], None)
                .unwrap()
                .trim(),
            sha
        );
        // e uma edição local depois disso continua funcionando e vai ao remoto
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("local".into());
                Ok(())
            })
            .unwrap();
        c.sg.sync_now().await.unwrap();
        assert_ne!(
            git::run(remote.path(), &["rev-parse", "main"], None)
                .unwrap()
                .trim(),
            sha
        );
    }

    #[tokio::test]
    async fn spec_alterado_por_fora_fica_pendente_de_deploy_ate_um_deploy() {
        let (c, _remote, url) = com_remoto().await;
        let porta_antes = crate::store::services::get(&c.db, "s1")
            .await
            .unwrap()
            .unwrap()
            .spec
            .port;
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| p.services[0].spec.port = porta_antes + 1000);
        ext.commit_push("muda a porta");
        let st = c.sg.sync_now().await.unwrap();
        // o DECLARADO mudou...
        assert_eq!(
            crate::store::services::get(&c.db, "s1")
                .await
                .unwrap()
                .unwrap()
                .spec
                .port,
            porta_antes + 1000
        );
        // ...e fica a marca de que o que roda ainda não acompanha
        assert_eq!(st.pending_deploy, vec!["s1".to_string()]);
        // um deploy limpa a marca (é o que `deploy::start` faz)
        crate::store::state_git_marks::clear_pending(&c.db, "s1")
            .await
            .unwrap();
        assert!(c.sg.status().await.unwrap().pending_deploy.is_empty());
    }

    #[tokio::test]
    async fn yaml_invalido_nao_e_aplicado_e_o_proximo_commit_valido_sim() {
        let (c, _remote, url) = com_remoto().await;
        let estado_antes = (*c.db.infra().snapshot()).clone();
        let ext = Externo::clone_de(&url);
        std::fs::write(
            ext.path("projects/p1/project.yml"),
            "isto: [não, fecha\n  : :",
        )
        .unwrap();
        let ruim = ext.commit_push("quebra o yaml");

        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(*c.db.infra().snapshot(), estado_antes, "nada foi aplicado");
        let rej = st.incoming_rejected.expect("a recusa fica visível");
        assert_eq!(rej.sha, ruim);
        assert!(rej.reason.contains("NÃO foi aplicado"), "{}", rej.reason);
        assert!(st.last_error.is_some());
        assert_ne!(head_local(&c), ruim);

        // o remoto conserta com um commit novo: o TOPO é válido e é aplicado
        git::run(
            ext.dir.path(),
            &["checkout", "-q", "HEAD~1", "--", "projects/p1/project.yml"],
            None,
        )
        .unwrap();
        ext.edita_projeto("p1", |p| p.description = Some("consertado".into()));
        let bom = ext.commit_push("conserta");
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.incoming_rejected, None);
        assert_eq!(st.last_error, None, "{st:?}");
        assert_eq!(head_local(&c), bom);
    }

    #[tokio::test]
    async fn valor_em_claro_vindo_de_fora_e_recusado_sem_ecoar_o_valor() {
        let (c, _remote, url) = com_remoto().await;
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| {
            p.env_vars.push(EnvVar {
                key: "STRIPE_SECRET_KEY".into(),
                value: EnvVarValue::Plain("sk_live_vazou".into()),
            })
        });
        ext.commit_push("põe segredo em claro");
        let st = c.sg.sync_now().await.unwrap();
        let rej = st.incoming_rejected.expect("recusado");
        assert!(rej.reason.contains("valor(es) em claro"), "{}", rej.reason);
        assert!(rej.reason.contains("STRIPE_SECRET_KEY"));
        assert!(
            !rej.reason.contains("sk_live_vazou")
                && !st.last_error.unwrap().contains("sk_live_vazou")
        );
        assert!(c.db.infra().snapshot().projects[0].env_vars.is_empty());
    }

    #[tokio::test]
    async fn ciclo_de_dependencias_vindo_de_fora_e_recusado() {
        let (c, _remote, url) = com_remoto().await;
        c.db.seed_service_for_test("p1", "s2");
        c.sg.sync_now().await.unwrap(); // commita e envia o s2
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| {
            for s in &mut p.services {
                s.spec.depends_on = vec![if s.id == "s1" {
                    "s2".into()
                } else {
                    "s1".into()
                }];
            }
        });
        ext.commit_push("cria um ciclo");
        let st = c.sg.sync_now().await.unwrap();
        let rej = st.incoming_rejected.expect("recusado");
        assert!(rej.reason.contains("ciclo"), "{}", rej.reason);
        assert!(
            c.db.infra().snapshot().projects[0]
                .services
                .iter()
                .all(|s| s.spec.depends_on.is_empty())
        );
    }

    #[tokio::test]
    async fn segredo_que_nao_decifra_neste_servidor_e_recusado() {
        let (c, _remote, url) = com_remoto().await;
        let outra = age::x25519::Identity::generate();
        let alheio = crate::platform::age_format::encrypt("x", &[outra.to_public()]).unwrap();
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| {
            p.secrets.push(crate::store::infra::model::SecretInfra {
                id: "sec_1".into(),
                key: "DB_PASSWORD".into(),
                value: alheio.clone(),
            })
        });
        ext.commit_push("secret de outro servidor");
        let st = c.sg.sync_now().await.unwrap();
        let rej = st.incoming_rejected.expect("recusado");
        assert!(rej.reason.contains("não decifra"), "{}", rej.reason);
        assert!(rej.reason.contains("DB_PASSWORD"));
    }

    #[tokio::test]
    async fn secret_cifrado_para_este_servidor_e_aceito() {
        let (c, _remote, url) = com_remoto().await;
        let ext = Externo::clone_de(&url);
        // é o que `rustploy-cli secret encrypt` faz: cifra com a chave PÚBLICA do servidor
        let enc = {
            let pk = c.secrets.public_key();
            crate::platform::age_format::encrypt(
                "segredo-bom",
                &[crate::platform::age_format::parse_recipient(&pk).unwrap()],
            )
            .unwrap()
        };
        ext.edita_projeto("p1", |p| {
            p.secrets.push(crate::store::infra::model::SecretInfra {
                id: "sec_1".into(),
                key: "DB_PASSWORD".into(),
                value: enc.clone(),
            })
        });
        ext.commit_push("secret cifrado com a chave pública");
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.incoming_rejected, None, "{st:?}");
        assert_eq!(
            c.secrets.get_raw("p1", "DB_PASSWORD").await.unwrap(),
            "segredo-bom"
        );
    }

    #[tokio::test]
    async fn remocao_por_fora_so_marca_e_nao_apaga_runtime_nem_derruba_nada() {
        let (c, _remote, url) = com_remoto().await;
        c.db.seed_service_for_test("p1", "s2");
        crate::store::services::update_status(
            &c.db,
            "s2",
            &shared::ServiceStatus::Running,
            Some("cont_vivo"),
        )
        .await
        .unwrap();
        c.sg.sync_now().await.unwrap();
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| p.services.retain(|s| s.id != "s2"));
        ext.commit_push("remove o s2 do yaml");
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.incoming_rejected, None, "{st:?}");
        // saiu do estado declarado...
        assert!(
            crate::store::services::get(&c.db, "s2")
                .await
                .unwrap()
                .is_none()
        );
        // ...mas ficou a marca, e o runtime (container vivo) NÃO foi tocado
        assert_eq!(st.removed_in_git.len(), 1);
        assert_eq!(st.removed_in_git[0].id, "serviço:s2");
        let (cont,): (Option<String>,) =
            sqlx::query_as("SELECT live_container_id FROM service_runtime WHERE service_id = 's2'")
                .fetch_one(c.db.pool())
                .await
                .unwrap();
        assert_eq!(cont.as_deref(), Some("cont_vivo"));
        // a pessoa esquece a marca
        c.sg.dismiss_removal("serviço:s2").await.unwrap();
        assert!(c.sg.status().await.unwrap().removed_in_git.is_empty());
        assert!(c.sg.dismiss_removal("serviço:s2").await.is_err());
    }

    #[tokio::test]
    async fn remover_um_projeto_inteiro_marca_o_projeto_e_os_servicos() {
        let (c, _remote, url) = com_remoto().await;
        c.db.seed_project_for_test("p2");
        c.db.seed_service_for_test("p2", "x1");
        c.sg.sync_now().await.unwrap();
        let ext = Externo::clone_de(&url);
        std::fs::remove_dir_all(ext.path("projects/p2")).unwrap();
        ext.commit_push("apaga o projeto p2");
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.incoming_rejected, None, "{st:?}");
        let ids: Vec<_> = st.removed_in_git.iter().map(|r| r.id.as_str()).collect();
        assert!(
            ids.contains(&"projeto:p2") && ids.contains(&"serviço:x1"),
            "{ids:?}"
        );
        assert!(
            crate::store::projects::get(&c.db, "p2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            crate::store::projects::get(&c.db, "p1")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn historico_divergido_pausa_a_rede_e_retomar_depois_de_resolver() {
        let (c, remote, url) = com_remoto().await;
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| p.description = Some("do colega".into()));
        let do_colega = ext.commit_push("colega edita");
        // enquanto isso, uma edição LOCAL diferente: os dois lados andaram
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("do servidor".into());
                Ok(())
            })
            .unwrap();
        let st = c.sg.sync_now().await.unwrap();
        let motivo = st.paused_reason.clone().expect("pausou");
        assert!(
            motivo.contains("divergiu") && motivo.contains("retomar"),
            "{motivo}"
        );
        assert_eq!((st.ahead, st.behind), (1, 1));
        // nada foi enviado nem aplicado
        assert_eq!(
            git::run(remote.path(), &["rev-parse", "main"], None)
                .unwrap()
                .trim(),
            do_colega
        );
        assert_eq!(
            crate::store::projects::get(&c.db, "p1")
                .await
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("do servidor")
        );
        // pausado, o daemon NÃO mexe no git (nem commita): quem resolve à mão por SSH não pode ter um
        // `git add -A` do daemon capturando um merge pela metade. As edições da interface continuam
        // valendo no estado e nos arquivos.
        let antes = head_local(&c);
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("do servidor 2".into());
                Ok(())
            })
            .unwrap();
        c.sg.cycle(false).await;
        assert_eq!(head_local(&c), antes, "pausado não commita");
        assert_eq!(
            crate::store::projects::get(&c.db, "p1")
                .await
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("do servidor 2")
        );
        assert_eq!(
            git::run(remote.path(), &["rev-parse", "main"], None)
                .unwrap()
                .trim(),
            do_colega
        );

        // a pessoa resolve no repositório (aqui: commita o que ficou e junta mantendo o lado do
        // servidor) e retoma
        git_out(&c, &["add", "-A"]);
        git_out(&c, &["commit", "-q", "-m", "à mão: o que ficou pendente"]);
        git_out(
            &c,
            &[
                "merge",
                "-q",
                "--no-edit",
                "-s",
                "ours",
                "refs/remotes/origin/main",
            ],
        );
        let st = c.sg.resume().await.unwrap();
        assert_eq!(st.paused_reason, None, "{st:?}");
        assert_eq!(st.last_error, None);
        assert_eq!(
            git::run(remote.path(), &["rev-parse", "main"], None)
                .unwrap()
                .trim(),
            head_local(&c),
            "retomar enviou o merge"
        );
    }

    #[tokio::test]
    async fn retomar_relê_a_pasta_depois_de_resolver_a_mao_e_recusa_pasta_quebrada() {
        let (c, remote, _url, do_colega) = divergido().await;
        let estado_do_servidor = (*c.db.infra().snapshot()).clone();

        // 1) a pessoa deixou um merge com marcador de conflito no YAML e mandou retomar
        let arq = infra_dir(&c).join("projects/p1/project.yml");
        let bom = std::fs::read_to_string(&arq).unwrap();
        std::fs::write(
            &arq,
            format!("<<<<<<< HEAD\n{bom}=======\noutra coisa\n>>>>>>> origin/main\n"),
        )
        .unwrap();
        let e = c.sg.resume().await.unwrap_err();
        assert!(e.to_string().contains("não está num estado válido"), "{e}");
        assert_eq!(
            *c.db.infra().snapshot(),
            estado_do_servidor,
            "nada mudou em memória"
        );
        assert!(
            c.sg.status().await.unwrap().paused_reason.is_some(),
            "continua pausado"
        );

        // 2) agora ela adota o remoto à mão (guarda o local numa branch e faz reset): o daemon precisa
        // reler a pasta, senão a próxima escrita desfaria o que foi resolvido
        git_out(&c, &["checkout", "-q", "--", "."]);
        git_out(&c, &["branch", "backup-manual", "main"]);
        git_out(&c, &["reset", "-q", "--hard", "refs/remotes/origin/main"]);
        let st = c.sg.resume().await.unwrap();
        assert_eq!(st.paused_reason, None, "{st:?}");
        assert_eq!(
            crate::store::projects::get(&c.db, "p1")
                .await
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("do colega"),
            "o estado em memória acompanhou a pasta"
        );
        assert_eq!(head_local(&c), do_colega);
        // uma escrita nova NÃO desfaz o que foi resolvido (o risco que o reload evita)
        c.db.infra()
            .update(|st| {
                st.settings.acme_email = Some("a@b.c".into());
                Ok(())
            })
            .unwrap();
        c.sg.cycle(true).await;
        let em_disco =
            std::fs::read_to_string(infra_dir(&c).join("projects/p1/project.yml")).unwrap();
        assert!(em_disco.contains("do colega"), "{em_disco}");
        assert!(
            git::run(
                remote.path(),
                &["show", "main:projects/p1/project.yml"],
                None
            )
            .unwrap()
            .contains("do colega")
        );
    }

    #[tokio::test]
    async fn conectar_um_repositorio_com_historia_propria_da_erro_em_vez_de_sobrescrever() {
        let c = cenario().await;
        let (remote, url) = bare_remote();
        // alguém já usa esse repositório para outra coisa
        let outro = Externo::clone_de(&url);
        std::fs::write(outro.path("README.md"), "outra coisa\n").unwrap();
        outro.commit_push("história que não é nossa");
        // erro claro na hora: nada de "ok" que depois vira pausa por divergência
        let e =
            c.sg.configure("sync", Some(url), None, None)
                .await
                .unwrap_err();
        assert!(matches!(e, StateGitError::Invalid(_)), "{e:?}");
        assert!(e.to_string().contains("servidor limpo"), "{e}");
        // a configuração não foi gravada: segue desligado
        assert_eq!(c.sg.settings().await.unwrap().mode, Mode::Off);
        // o remoto não foi tocado
        assert!(
            git::run(remote.path(), &["log", "-1", "--format=%s", "main"], None)
                .unwrap()
                .contains("não é nossa")
        );
    }

    #[tokio::test]
    async fn reconfigurar_o_mesmo_remoto_de_um_sync_que_ja_funcionava_nao_da_erro() {
        let (c, _remote, url) = com_remoto().await;
        c.sg.sync_now().await.unwrap();
        // ex.: trocar o token do provider: o repositório tem a história que este servidor conhece
        let st = c.sg.configure("sync", Some(url), None, None).await.unwrap();
        assert_eq!(st.paused_reason, None, "{st:?}");
    }

    #[tokio::test]
    async fn rollback_no_git_devolve_o_servico_e_a_marca_de_removido_some() {
        let (c, _remote, url) = com_remoto().await;
        c.db.seed_service_for_test("p1", "s2");
        crate::store::services::update_status(
            &c.db,
            "s2",
            &shared::ServiceStatus::Running,
            Some("cont_vivo"),
        )
        .await
        .unwrap();
        c.sg.sync_now().await.unwrap();
        let ext = Externo::clone_de(&url);
        let antes = head_remoto(&ext);
        ext.edita_projeto("p1", |p| p.services.retain(|s| s.id != "s2"));
        ext.commit_push("remove o s2");
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.removed_in_git.len(), 1);
        assert!(
            crate::store::services::get(&c.db, "s2")
                .await
                .unwrap()
                .is_none()
        );

        // rollback: `git revert` do commit que removeu (o s2 volta com o MESMO id)
        git::run(ext.dir.path(), &["revert", "--no-edit", "HEAD"], None).unwrap();
        git::run(
            ext.dir.path(),
            &["push", "-q", "origin", "HEAD:refs/heads/main"],
            None,
        )
        .unwrap();
        let _ = antes;
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.incoming_rejected, None, "{st:?}");
        assert!(
            st.removed_in_git.is_empty(),
            "a marca some quando o serviço volta: {st:?}"
        );
        let svc = crate::store::services::get(&c.db, "s2")
            .await
            .unwrap()
            .unwrap();
        // o runtime nunca foi tocado: o serviço volta já apontando para o mesmo container
        assert_eq!(svc.live_container_id.as_deref(), Some("cont_vivo"));
        assert_eq!(svc.status, shared::ServiceStatus::Running);
    }

    fn head_remoto(ext: &Externo) -> String {
        git::run(ext.dir.path(), &["rev-parse", "HEAD"], None)
            .unwrap()
            .trim()
            .to_string()
    }

    /// Deixa o cenário divergido: o colega edita no remoto e o servidor edita localmente.
    async fn divergido() -> (Cenario, tempfile::TempDir, String, String) {
        let (c, remote, url) = com_remoto().await;
        let ext = Externo::clone_de(&url);
        ext.edita_projeto("p1", |p| p.description = Some("do colega".into()));
        let do_colega = ext.commit_push("colega edita");
        c.db.infra()
            .update(|st| {
                st.projects[0].description = Some("do servidor".into());
                Ok(())
            })
            .unwrap();
        let st = c.sg.sync_now().await.unwrap();
        assert!(st.paused_reason.is_some(), "{st:?}");
        (c, remote, url, do_colega)
    }

    #[tokio::test]
    async fn usar_o_do_remoto_adota_o_repositorio_e_guarda_o_local_numa_branch() {
        let (c, remote, _url, do_colega) = divergido().await;
        let local_antes = head_local(&c);
        let st =
            c.sg.resolve(shared::StateGitResolveChoice::UseRemote)
                .await
                .unwrap();
        assert_eq!((st.paused_reason, st.last_error), (None, None));
        // o servidor agora tem o conteúdo do colega...
        assert_eq!(
            crate::store::projects::get(&c.db, "p1")
                .await
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("do colega")
        );
        assert_eq!(head_local(&c), do_colega);
        // ...e o que era só do servidor está salvo numa branch de backup
        let branches = git_out(&c, &["branch", "--list", "rustploy-backup/local-*"]);
        assert!(branches.contains("rustploy-backup/local-"), "{branches}");
        let backup = branches.trim().trim_start_matches("* ").to_string();
        assert_eq!(git_out(&c, &["rev-parse", &backup]).trim(), local_antes);
        assert!(
            git_out(&c, &["show", &format!("{backup}:projects/p1/project.yml")])
                .contains("do servidor")
        );
        // o remoto não foi tocado
        assert_eq!(
            git::run(remote.path(), &["rev-parse", "main"], None)
                .unwrap()
                .trim(),
            do_colega
        );
    }

    #[tokio::test]
    async fn usar_o_meu_sobrescreve_o_remoto_depois_de_guardar_o_dele() {
        let (c, remote, _url, do_colega) = divergido().await;
        let st =
            c.sg.resolve(shared::StateGitResolveChoice::UseLocal)
                .await
                .unwrap();
        assert_eq!(
            (&st.paused_reason, &st.last_error),
            (&None, &None),
            "{st:?}"
        );
        // o remoto agora é o do servidor
        assert_eq!(
            git::run(remote.path(), &["rev-parse", "main"], None)
                .unwrap()
                .trim(),
            head_local(&c)
        );
        assert!(
            git::run(
                remote.path(),
                &["show", "main:projects/p1/project.yml"],
                None
            )
            .unwrap()
            .contains("do servidor")
        );
        // e o que o remoto tinha ficou numa branch de backup NO REMOTO
        let backups = git::run(
            remote.path(),
            &["branch", "--list", "rustploy-backup/remote-*"],
            None,
        )
        .unwrap();
        let nome = backups.trim().trim_start_matches("* ").to_string();
        assert!(!nome.is_empty(), "{backups}");
        assert_eq!(
            git::run(remote.path(), &["rev-parse", &nome], None)
                .unwrap()
                .trim(),
            do_colega
        );
        // o estado local não mudou
        assert_eq!(
            crate::store::projects::get(&c.db, "p1")
                .await
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("do servidor")
        );
    }

    #[tokio::test]
    async fn resolver_so_vale_pausado_e_em_sync_e_um_remoto_invalido_nao_e_adotado() {
        // sem pausa: nada a resolver
        let (c, _remote, _url) = com_remoto().await;
        assert!(
            c.sg.resolve(shared::StateGitResolveChoice::UseLocal)
                .await
                .is_err()
        );
        // em modo commit também não
        let c2 = cenario().await;
        c2.sg.configure("commit", None, None, None).await.unwrap();
        assert!(
            c2.sg
                .resolve(shared::StateGitResolveChoice::UseRemote)
                .await
                .is_err()
        );

        // repositório com história de OUTRA coisa nem chega a ser configurado (erro no configure),
        // então não há pausa para "resolver"
        let c3 = cenario().await;
        let (_remote, url) = bare_remote();
        let outro = Externo::clone_de(&url);
        std::fs::write(outro.path("README.md"), "outra coisa\n").unwrap();
        outro.commit_push("história que não é nossa");
        assert!(
            c3.sg
                .configure("sync", Some(url), None, None)
                .await
                .is_err()
        );
        assert!(
            c3.sg
                .resolve(shared::StateGitResolveChoice::UseRemote)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn recusa_some_quando_o_remoto_volta_ao_que_o_servidor_ja_tem() {
        let (c, _remote, url) = com_remoto().await;
        let ext = Externo::clone_de(&url);
        let bom = head_remoto(&ext);
        std::fs::write(ext.path("projects/p1/project.yml"), "quebrado: [").unwrap();
        ext.commit_push("quebra");
        let st = c.sg.sync_now().await.unwrap();
        assert!(st.incoming_rejected.is_some());
        // o remoto é reescrito de volta ao commit bom (igual ao servidor): não há mais o que recusar
        git::run(ext.dir.path(), &["reset", "-q", "--hard", &bom], None).unwrap();
        git::run(
            ext.dir.path(),
            &["push", "-q", "-f", "origin", "HEAD:refs/heads/main"],
            None,
        )
        .unwrap();
        let st = c.sg.sync_now().await.unwrap();
        assert_eq!(st.incoming_rejected, None, "{st:?}");
        assert_eq!(st.last_error, None, "{st:?}");
    }

    /// Fase 8: servidor limpo + repositório com histórico → o estado volta, sem pausar por
    /// "históricos divergentes" (o servidor novo nunca teve história própria).
    #[tokio::test]
    async fn servidor_limpo_restaura_o_estado_do_repositorio() {
        let a = cenario().await;
        add_env(&a, "FOO", "bar");
        a.db.seed_service_for_test("p1", "svc_a");
        let (_remote_dir, url) = bare_remote();
        let st =
            a.sg.configure("sync", Some(url.clone()), None, None)
                .await
                .unwrap();
        assert_eq!(st.last_push_ok, Some(true), "{:?}", st.last_error);

        let b = cenario_limpo().await;
        assert!(
            crate::store::projects::list(&b.db)
                .await
                .unwrap()
                .is_empty()
        );
        let religou = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = religou.clone();
        b.sg.set_after_restore(move || flag.store(true, std::sync::atomic::Ordering::SeqCst));
        let st = b.sg.configure("sync", Some(url), None, None).await.unwrap();
        assert!(
            st.paused_reason.is_none(),
            "não deveria pausar: {:?}",
            st.last_error
        );
        assert!(st.last_error.is_none(), "{:?}", st.last_error);
        let projetos = crate::store::projects::list(&b.db).await.unwrap();
        assert_eq!(projetos.len(), 1, "o projeto do repositório volta");
        assert_eq!(projetos[0].id, "p1");
        assert_eq!(
            git_out(&b, &["rev-parse", "HEAD"]).trim(),
            git_out(&a, &["rev-parse", "HEAD"]).trim(),
            "o servidor novo segue a mesma história"
        );
        assert_eq!((st.ahead, st.behind), (0, 0));
        assert!(
            religou.load(std::sync::atomic::Ordering::SeqCst),
            "a restauração religa o que voltou"
        );
        assert_eq!(
            crate::store::services::desired_state(&b.db, "svc_a")
                .await
                .unwrap(),
            crate::store::services::DesiredState::Running
        );
    }

    /// Um servidor que JÁ tem projetos nunca é sobrescrito, e também não finge que deu certo:
    /// ligar o sync num repositório com história sem relação com a dele é um erro.
    #[tokio::test]
    async fn servidor_com_estado_proprio_nao_e_sobrescrito_pelo_remoto() {
        let a = cenario().await;
        let (_remote_dir, url) = bare_remote();
        a.sg.configure("sync", Some(url.clone()), None, None)
            .await
            .unwrap();
        let b = cenario().await; // tem p1 próprio, história própria
        add_env(&b, "LOCAL", "1");
        let e =
            b.sg.configure("sync", Some(url), None, None)
                .await
                .unwrap_err();
        assert!(matches!(e, StateGitError::Invalid(_)), "{e:?}");
        assert_eq!(b.sg.settings().await.unwrap().mode, Mode::Off);
    }
}
