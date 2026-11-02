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
use shared::{StateGitCommit, StateGitStatus};
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
/// Mesmo sem mudança, tenta de novo o que falhou (push) neste intervalo.
const RETRY_EVERY: Duration = Duration::from_secs(300);

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

#[derive(Default)]
struct Runtime {
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
}

fn fingerprint(project: &str, scope: &str, what: &str) -> String {
    format!("{project}\u{1f}{scope}\u{1f}{what}")
}

/// Achados que **impedem** o commit: as certezas sempre, e as suspeitas que o usuário não liberou.
fn blocking(state: &InfraState, allow: &BTreeSet<String>) -> Vec<Finding> {
    plaintext_findings(state)
        .into_iter()
        .filter(|f| {
            f.certainty == Certainty::Definite
                || !allow.contains(&fingerprint(&f.project, &f.scope, &f.what))
        })
        .collect()
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
        Self::with_debounce(db, secrets, DEBOUNCE)
    }

    pub fn with_debounce(
        db: Arc<Db>,
        secrets: Arc<SecretsManager>,
        debounce: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            debounce,
            db,
            secrets,
            rt: Mutex::new(Runtime::default()),
            wake: tokio::sync::Notify::new(),
            cycle_lock: tokio::sync::Mutex::new(()),
        })
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
            rt.needs_push = mode == Mode::Sync;
        }
        if mode != Mode::Off {
            // O primeiro commit já nasce limpo (a auditoria acabou de passar).
            self.cycle(true).await;
        }
        Ok(self.status().await?)
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

    async fn cycle_inner(&self, force_push: bool) -> anyhow::Result<()> {
        let s = self.settings().await?;
        if s.mode == Mode::Off {
            return Ok(());
        }
        let allow = self.allow_set().await?;

        // Commit: dentro do lock de escrita, para o `git` só ver o diretório entre transações e para
        // a auditoria valer para exatamente o que será commitado.
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

        let (committed, push_wanted) = match step {
            Step::Blocked(n) => {
                let mut rt = self.rt.lock().unwrap();
                rt.blocked = n;
                rt.last_error = Some(format!(
                    "commit suspenso: {n} valor(es) em claro no estado (veja InfraAudit)"
                ));
                return Ok(());
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
                (
                    committed,
                    s.mode == Mode::Sync && (rt.needs_push || force_push),
                )
            }
        };
        let _ = committed;

        if push_wanted {
            self.push(&s).await;
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
            let mut tick = tokio::time::interval(RETRY_EVERY);
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
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::store::connect(&dir.path().join("data"))
                .await
                .unwrap(),
        );
        let secrets =
            Arc::new(SecretsManager::new(&dir.path().join("master.key"), db.clone()).unwrap());
        db.seed_project_for_test("p1");
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
        assert_eq!(st.last_push_ok, Some(false));
        assert!(st.needs_push);
        assert!(st.last_error.unwrap().contains("push falhou"));
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
}
