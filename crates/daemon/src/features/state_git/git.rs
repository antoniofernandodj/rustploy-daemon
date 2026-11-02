//! Invólucro fino do binário `git` do sistema (decisão da R9.0: nada de `git2`/`reqwest`).
//!
//! Regras de segurança, todas aqui para ficarem num lugar só:
//! - a credencial vai ao `git` **só por variável de ambiente** (`http.extraHeader` via
//!   `GIT_CONFIG_*`), nunca na URL do remote, em argumento (vazaria no `ps`) nem em `.git/config`;
//! - o `git` não lê a config do usuário nem do sistema, não abre prompt e não usa credential helper;
//! - tudo o que volta do `git` para o log/UI passa por [`redact`], que troca o token por `***`.
//!
//! Funções **síncronas** (`std::process`): quem as chama dentro de código async usa `spawn_blocking`.

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use std::path::Path;
use std::process::{Command, Stdio};

/// Credencial HTTPS (usuário + token) para falar com o remoto.
#[derive(Clone)]
pub struct Auth {
    pub username: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    /// RFC 3339.
    pub at: String,
    pub subject: String,
}

/// O `git` existe e responde? Resultado guardado: o snapshot pergunta isto a cada atualização, e
/// executar `git --version` toda vez seria desperdício. (Instalar o `git` depois exige reiniciar o
/// daemon para a tela notar.)
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Troca o token (e a sua forma em base64, que é o que vai no cabeçalho) por `***`.
pub fn redact(text: &str, auth: Option<&Auth>) -> String {
    let Some(a) = auth else {
        return text.to_string();
    };
    let header = B64.encode(format!("{}:{}", a.username, a.token));
    let mut out = text.replace(&header, "***");
    if !a.token.is_empty() {
        out = out.replace(&a.token, "***");
    }
    out
}

fn command(dir: &Path, auth: Option<&Auth>) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir)
        // não ler ~/.gitconfig nem /etc/gitconfig: o comportamento não pode depender do host
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_AUTHOR_NAME", "rustploy")
        .env("GIT_AUTHOR_EMAIL", "rustploy@localhost")
        .env("GIT_COMMITTER_NAME", "rustploy")
        .env("GIT_COMMITTER_EMAIL", "rustploy@localhost")
        .stdin(Stdio::null());
    match auth {
        Some(a) => {
            let basic = B64.encode(format!("{}:{}", a.username, a.token));
            c.env("GIT_CONFIG_COUNT", "2")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                )
                .env("GIT_CONFIG_KEY_1", "credential.helper")
                .env("GIT_CONFIG_VALUE_1", "");
        }
        None => {
            c.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "credential.helper")
                .env("GIT_CONFIG_VALUE_0", "");
        }
    }
    c
}

/// Roda `git <args>` em `dir` e devolve o stdout. Em caso de erro, a mensagem traz o stderr **já
/// sem o token**.
pub fn run(dir: &Path, args: &[&str], auth: Option<&Auth>) -> Result<String> {
    let out = command(dir, auth)
        .args(args)
        .output()
        .with_context(|| "não consegui executar o `git` (está instalado?)".to_string())?;
    if !out.status.success() {
        let stderr = redact(&String::from_utf8_lossy(&out.stderr), auth);
        let stdout = redact(&String::from_utf8_lossy(&out.stdout), auth);
        let detail = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            detail.trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Remoto aceito: `https://` sem credencial embutida, ou `file://`/caminho absoluto (cópia local e
/// testes). `ssh://` e `git@host:` não são suportados: a credencial é o token do provider.
pub fn validate_remote(url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        bail!("o endereço do repositório está vazio");
    }
    if url.starts_with("file://") || url.starts_with('/') {
        return Ok(());
    }
    let Some(rest) = url.strip_prefix("https://") else {
        bail!(
            "só são aceitos endereços https:// (a credencial vem do provider git cadastrado), file:// ou um caminho absoluto: {}",
            strip_userinfo(url)
        );
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        bail!("o endereço não pode conter usuário/senha: use o provider git para a credencial");
    }
    if authority.is_empty() || url.chars().any(|c| c.is_whitespace()) {
        bail!("endereço do repositório inválido");
    }
    Ok(())
}

/// Para mensagens de erro: nunca ecoa `usuario:senha@`.
fn strip_userinfo(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

const GITIGNORE: &str = ".lock\n.staging/\n";

/// Garante o repositório em `dir`: `git init`, branch certa, `.gitignore` e o remote `origin`.
pub fn ensure_repo(dir: &Path, branch: &str, remote: Option<&str>) -> Result<()> {
    if branch.is_empty() || branch.starts_with('-') || branch.chars().any(|c| c.is_whitespace()) {
        bail!("nome de branch inválido: {branch:?}");
    }
    if !dir.join(".git").exists() {
        run(dir, &["init", "-q"], None)?;
        run(
            dir,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
            None,
        )?;
    } else if run(dir, &["symbolic-ref", "--short", "HEAD"], None)?.trim() != branch {
        run(dir, &["checkout", "-q", "-B", branch], None)?;
    }
    let ignore = dir.join(".gitignore");
    if std::fs::read_to_string(&ignore).ok().as_deref() != Some(GITIGNORE) {
        std::fs::write(&ignore, GITIGNORE).context("gravando .gitignore")?;
    }
    let current = run(dir, &["remote", "get-url", "origin"], None)
        .ok()
        .map(|s| s.trim().to_string());
    match (remote, current) {
        (Some(url), None) => {
            run(dir, &["remote", "add", "origin", url], None)?;
        }
        (Some(url), Some(cur)) if cur != url => {
            run(dir, &["remote", "set-url", "origin", url], None)?;
        }
        (None, Some(_)) => {
            run(dir, &["remote", "remove", "origin"], None)?;
        }
        _ => {}
    }
    Ok(())
}

/// O último commit, ou `None` se o repositório ainda não tem nenhum.
pub fn head(dir: &Path) -> Result<Option<CommitInfo>> {
    let Ok(out) = run(dir, &["log", "-1", "--format=%H%x1f%cI%x1f%s"], None) else {
        return Ok(None);
    };
    let mut parts = out.trim_end().splitn(3, '\u{1f}');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(sha), Some(at), Some(subject)) if !sha.is_empty() => Ok(Some(CommitInfo {
            sha: sha.into(),
            at: at.into(),
            subject: subject.into(),
        })),
        _ => Ok(None),
    }
}

/// `git add -A` e, se houver diferença, um commit. A mensagem é montada a partir dos caminhos
/// alterados. **Sem diff, sem commit** (devolve `None`).
pub fn commit_all(
    dir: &Path,
    message: impl FnOnce(&[String]) -> String,
) -> Result<Option<CommitInfo>> {
    run(dir, &["add", "-A"], None)?;
    let changed: Vec<String> = run(dir, &["diff", "--cached", "--name-only"], None)?
        .lines()
        .map(str::to_string)
        .collect();
    if changed.is_empty() {
        return Ok(None);
    }
    let msg = message(&changed);
    run(dir, &["commit", "-q", "-m", &msg], None)?;
    head(dir)
}

/// `git push origin <branch>`. Falha de rede/credencial/não-fast-forward volta como erro (já sem token).
pub fn push(dir: &Path, branch: &str, auth: Option<&Auth>) -> Result<()> {
    run(
        dir,
        &[
            "push",
            "-q",
            "origin",
            &format!("{branch}:refs/heads/{branch}"),
        ],
        auth,
    )
    .map(|_| ())
    .map_err(|e| anyhow!("{e}"))
}

/// Como [`run`], mas devolve os **bytes** do stdout (conteúdo de arquivo: não pode passar por UTF-8
/// com perda).
pub fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = command(dir, None)
        .args(args)
        .output()
        .with_context(|| "não consegui executar o `git` (está instalado?)".to_string())?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// `git fetch` da branch para `refs/remotes/origin/<branch>`. `Ok(false)` = o remoto ainda **não
/// tem** essa branch (repositório novo/vazio): não é erro, é "nada a trazer".
pub fn fetch(dir: &Path, branch: &str, auth: Option<&Auth>) -> Result<bool> {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    match run(dir, &["fetch", "-q", "origin", &refspec], auth) {
        Ok(_) => Ok(true),
        Err(e) => {
            let m = e.to_string();
            if m.contains("couldn't find remote ref") || m.contains("Couldn't find remote ref") {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// O commit em `refs/remotes/origin/<branch>`, se existir.
pub fn remote_head(dir: &Path, branch: &str) -> Option<String> {
    run(
        dir,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/remotes/origin/{branch}"),
        ],
        None,
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// `(à frente, atrás)`: commits locais que o remoto não tem, e commits do remoto que o local não
/// tem. `(0, 0)` = iguais. Ambos > 0 = **histórico divergiu**. Sem ancestral comum (o repositório
/// local foi iniciado do zero e o remoto já tinha história) também dá ambos > 0.
pub fn ahead_behind(dir: &Path, branch: &str) -> Result<(usize, usize)> {
    let out = run(
        dir,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...refs/remotes/origin/{branch}"),
        ],
        None,
    )?;
    let mut it = out.split_whitespace();
    let ahead = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let behind = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    Ok((ahead, behind))
}

/// Não há nada por commitar nem arquivo novo/alterado/apagado (fora o que o `.gitignore` esconde)?
pub fn is_clean(dir: &Path) -> Result<bool> {
    Ok(run(dir, &["status", "--porcelain"], None)?
        .trim()
        .is_empty())
}

/// Os arquivos do commit `rev`, **sem fazer checkout**: `caminho → bytes`. Só os que importam ao
/// estado (`is_state_file`), para um `README` do repositório não atrapalhar.
pub fn files_at(
    dir: &Path,
    rev: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let names = run(dir, &["ls-tree", "-r", "--name-only", rev], None)?;
    let mut out = std::collections::BTreeMap::new();
    for rel in names.lines().filter(|r| keep(r)) {
        out.insert(
            rel.to_string(),
            run_bytes(dir, &["cat-file", "blob", &format!("{rev}:{rel}")])?,
        );
    }
    Ok(out)
}

/// `git branch <nome> <rev>`: guarda um commit (ex.: o histórico local que vai ser descartado).
pub fn backup_branch(dir: &Path, name: &str, rev: &str) -> Result<()> {
    run(dir, &["branch", "-f", name, rev], None).map(|_| ())
}

/// Copia `sha` para a branch `refs/heads/<name>` **do remoto** (um backup ao lado do que vai ser
/// sobrescrito), sem tocar na branch principal.
pub fn push_backup_ref(dir: &Path, sha: &str, name: &str, auth: Option<&Auth>) -> Result<()> {
    run(
        dir,
        &["push", "-q", "origin", &format!("{sha}:refs/heads/{name}")],
        auth,
    )
    .map(|_| ())
}

/// `push --force-with-lease`: sobrescreve a branch do remoto **só se ela ainda estiver em
/// `expected`** (o que o `fetch` viu). Se alguém empurrou nesse meio-tempo, recusa.
pub fn push_force_with_lease(
    dir: &Path,
    branch: &str,
    expected: &str,
    auth: Option<&Auth>,
) -> Result<()> {
    run(
        dir,
        &[
            "push",
            "-q",
            &format!("--force-with-lease=refs/heads/{branch}:{expected}"),
            "origin",
            &format!("{branch}:refs/heads/{branch}"),
        ],
        auth,
    )
    .map(|_| ())
}

/// Move a branch e a árvore de trabalho para `rev` (o estado já foi gravado, byte a byte igual).
pub fn reset_hard(dir: &Path, rev: &str) -> Result<()> {
    run(dir, &["reset", "-q", "--hard", rev], None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn git_esta_disponivel_no_ambiente_de_teste() {
        assert!(available(), "os testes da Fase 6 precisam do binário `git`");
    }

    #[test]
    fn remotos_aceitos_e_recusados() {
        for ok in [
            "https://github.com/org/estado.git",
            "file:///tmp/x.git",
            "/srv/git/estado.git",
        ] {
            assert!(validate_remote(ok).is_ok(), "{ok}");
        }
        for ruim in [
            "",
            "git@github.com:org/estado.git",
            "ssh://git@github.com/org/estado.git",
            "http://github.com/org/estado.git",
            "https://user:senha@github.com/org/estado.git",
            "https://token@github.com/org/estado.git",
            "relativo/caminho",
        ] {
            assert!(validate_remote(ruim).is_err(), "{ruim}");
        }
        // a mensagem de erro não ecoa a senha
        let e = validate_remote("ssh://user:senha-secreta@host/x")
            .unwrap_err()
            .to_string();
        assert!(!e.contains("senha-secreta"), "{e}");
    }

    #[test]
    fn redact_troca_token_e_cabecalho() {
        let a = Auth {
            username: "u".into(),
            token: "tok-secreto".into(),
        };
        let basic = B64.encode("u:tok-secreto");
        let t = redact(&format!("falhou com tok-secreto e Basic {basic}"), Some(&a));
        assert!(!t.contains("tok-secreto") && !t.contains(&basic));
        assert_eq!(redact("nada", None), "nada");
    }

    #[test]
    fn credencial_vai_so_por_variavel_de_ambiente_nunca_em_argumento() {
        let a = Auth {
            username: "x-access-token".into(),
            token: "tok-super-secreto".into(),
        };
        let mut c = command(Path::new("/tmp"), Some(&a));
        c.args(["push", "-q", "origin", "main:refs/heads/main"]);
        // nenhum argumento do processo (visível no `ps`) carrega o token nem o cabeçalho
        let basic = B64.encode("x-access-token:tok-super-secreto");
        for arg in c.get_args() {
            let arg = arg.to_string_lossy();
            assert!(
                !arg.contains("tok-super-secreto") && !arg.contains(&basic),
                "{arg}"
            );
        }
        let env: std::collections::BTreeMap<_, _> = c
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        // o cabeçalho está no ambiente...
        assert_eq!(env["GIT_CONFIG_KEY_0"].as_deref(), Some("http.extraHeader"));
        assert_eq!(
            env["GIT_CONFIG_VALUE_0"].as_deref(),
            Some(format!("Authorization: Basic {basic}").as_str())
        );
        // ...o git não pergunta nada e ignora a config do usuário/sistema/credential helper
        assert_eq!(env["GIT_TERMINAL_PROMPT"].as_deref(), Some("0"));
        assert_eq!(env["GIT_CONFIG_GLOBAL"].as_deref(), Some("/dev/null"));
        assert_eq!(env["GIT_CONFIG_NOSYSTEM"].as_deref(), Some("1"));
        assert_eq!(
            env["GIT_CONFIG_KEY_1"].as_deref(),
            Some("credential.helper")
        );
        // sem credencial, nada de cabeçalho
        let semcred = command(Path::new("/tmp"), None);
        let envs: Vec<_> = semcred
            .get_envs()
            .filter_map(|(k, _)| k.to_str().map(String::from))
            .collect();
        assert!(!envs.iter().any(|k| k == "GIT_CONFIG_VALUE_1"));
        assert!(
            semcred
                .get_envs()
                .all(|(_, v)| v != Some(std::ffi::OsStr::new("http.extraHeader")))
        );
    }

    #[test]
    fn init_commit_sem_diff_nao_commita_e_branch_certa() {
        let d = tmp();
        std::fs::write(d.path().join("a.yml"), "a: 1\n").unwrap();
        ensure_repo(d.path(), "estado", None).unwrap();
        assert_eq!(
            run(d.path(), &["symbolic-ref", "--short", "HEAD"], None)
                .unwrap()
                .trim(),
            "estado"
        );
        assert!(head(d.path()).unwrap().is_none());
        let c = commit_all(d.path(), |p| format!("muda {}", p.join(", ")))
            .unwrap()
            .unwrap();
        assert!(c.subject.starts_with("muda "), "{}", c.subject);
        // sem mudança → sem commit
        assert!(commit_all(d.path(), |_| "x".into()).unwrap().is_none());
        // .lock e .staging nunca entram
        std::fs::write(d.path().join(".lock"), "").unwrap();
        std::fs::create_dir_all(d.path().join(".staging/t")).unwrap();
        std::fs::write(d.path().join(".staging/t/f"), "x").unwrap();
        assert!(commit_all(d.path(), |_| "x".into()).unwrap().is_none());
        let tracked = run(d.path(), &["ls-files"], None).unwrap();
        assert!(
            !tracked.contains(".lock") && !tracked.contains(".staging"),
            "{tracked}"
        );
    }

    #[test]
    fn push_para_repositorio_local_e_remoto_mutavel() {
        let remote = tmp();
        run(remote.path(), &["init", "-q", "--bare"], None).unwrap();
        let url = format!("file://{}", remote.path().display());
        let d = tmp();
        std::fs::write(d.path().join("a.yml"), "a: 1\n").unwrap();
        ensure_repo(d.path(), "main", Some(&url)).unwrap();
        commit_all(d.path(), |_| "um".into()).unwrap().unwrap();
        push(d.path(), "main", None).unwrap();
        let no_remoto = run(remote.path(), &["log", "-1", "--format=%s", "main"], None).unwrap();
        assert_eq!(no_remoto.trim(), "um");
        // trocar o remoto atualiza o origin; tirar remove
        ensure_repo(d.path(), "main", Some("file:///outro.git")).unwrap();
        assert_eq!(
            run(d.path(), &["remote", "get-url", "origin"], None)
                .unwrap()
                .trim(),
            "file:///outro.git"
        );
        ensure_repo(d.path(), "main", None).unwrap();
        assert!(run(d.path(), &["remote", "get-url", "origin"], None).is_err());
    }

    #[test]
    fn push_que_falha_da_erro_sem_derrubar_nada() {
        let d = tmp();
        std::fs::write(d.path().join("a.yml"), "a\n").unwrap();
        ensure_repo(d.path(), "main", Some("file:///nao/existe.git")).unwrap();
        commit_all(d.path(), |_| "um".into()).unwrap().unwrap();
        assert!(push(d.path(), "main", None).is_err());
        // o commit local continua lá
        assert!(head(d.path()).unwrap().is_some());
    }
}
