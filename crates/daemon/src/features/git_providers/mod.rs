//! Clients for hosted Git providers. Gitea and GitHub are implemented; the
//! module is structured so GitLab (etc.) can be added alongside them. The
//! per-provider REST clients live in `gitea`/`github`; the free functions below
//! dispatch on [`shared::GitProviderKind`] so callers stay provider-agnostic.

pub mod gitea;
pub mod github;

use crate::platform::secrets::SecretsManager;
use crate::store::Db;
use crate::store::git_providers::{self, StoredProvider};
use anyhow::{Context, Result};
use shared::{GitAccount, GitBranch, GitProviderKind, GitRepo};

/// Reads a stored provider's kind, defaulting to Gitea for forward-compat with
/// rows written before a kind existed.
fn kind_of(p: &StoredProvider) -> GitProviderKind {
    GitProviderKind::from_str(&p.kind).unwrap_or(GitProviderKind::Gitea)
}

/// OAuth tokens, unified across providers.
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

// ── Provider-agnostic dispatch ────────────────────────────────────────────

pub async fn current_user(
    kind: GitProviderKind,
    base_url: &str,
    token: &str,
) -> Result<GitAccount> {
    match kind {
        GitProviderKind::Gitea => gitea::current_user(base_url, token).await,
        GitProviderKind::Github => github::current_user(base_url, token).await,
    }
}

pub async fn list_repos(
    kind: GitProviderKind,
    base_url: &str,
    token: &str,
) -> Result<Vec<GitRepo>> {
    match kind {
        GitProviderKind::Gitea => gitea::list_repos(base_url, token).await,
        GitProviderKind::Github => github::list_repos(base_url, token).await,
    }
}

pub async fn list_branches(
    kind: GitProviderKind,
    base_url: &str,
    token: &str,
    repo_full_name: &str,
) -> Result<Vec<GitBranch>> {
    match kind {
        GitProviderKind::Gitea => gitea::list_branches(base_url, token, repo_full_name).await,
        GitProviderKind::Github => github::list_branches(base_url, token, repo_full_name).await,
    }
}

pub async fn exchange_code(
    kind: GitProviderKind,
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<OAuthTokens> {
    match kind {
        GitProviderKind::Gitea => {
            let t = gitea::exchange_code(base_url, client_id, client_secret, code, redirect_uri)
                .await?;
            Ok(OAuthTokens {
                access_token: t.access_token,
                refresh_token: t.refresh_token,
            })
        }
        GitProviderKind::Github => {
            let t = github::exchange_code(base_url, client_id, client_secret, code, redirect_uri)
                .await?;
            Ok(OAuthTokens {
                access_token: t.access_token,
                refresh_token: t.refresh_token,
            })
        }
    }
}

/// Ensures `redirect_uri` is registered on the provider's OAuth app when the
/// provider supports it (Gitea does; GitHub is a no-op — its callback URLs are
/// not editable via API).
pub async fn ensure_redirect_uri(
    kind: GitProviderKind,
    base_url: &str,
    token: &str,
    client_id: &str,
    redirect_uri: &str,
) -> Result<()> {
    match kind {
        GitProviderKind::Gitea => {
            gitea::ensure_redirect_uri(base_url, token, client_id, redirect_uri).await
        }
        GitProviderKind::Github => {
            github::ensure_redirect_uri(base_url, token, client_id, redirect_uri).await
        }
    }
}

/// Builds the browser authorization URL. `client_id`/`redirect_uri`/`state`
/// arrive already percent-encoded.
pub fn authorize_url(
    kind: GitProviderKind,
    base_url: &str,
    client_id_enc: &str,
    redirect_uri_enc: &str,
    state_enc: &str,
) -> String {
    match kind {
        GitProviderKind::Gitea => format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&state={}",
            base_url.trim_end_matches('/'),
            client_id_enc,
            redirect_uri_enc,
            state_enc,
        ),
        GitProviderKind::Github => {
            github::authorize_url(base_url, client_id_enc, redirect_uri_enc, state_enc)
        }
    }
}

/// Path segment of the OAuth callback for a provider kind — `gitea` or `github`.
/// The daemon serves both under `{public_base}/oauth/{seg}/callback`.
pub fn callback_path_segment(kind: GitProviderKind) -> &'static str {
    kind.as_str()
}

/// Decrypts the access token (OAuth) or PAT a provider authenticates with.
/// Both auth modes store their bearer in `access_token_enc`.
pub fn usable_token(secrets: &SecretsManager, p: &StoredProvider) -> Result<String> {
    let enc = p
        .access_token_enc
        .as_deref()
        .context("provider Git não conectado (sem token)")?;
    secrets.decrypt(enc)
}

/// Host e porta de uma URL `https://` (sem usuário/senha, em minúsculas, porta 443 por padrão).
///
/// Serve para comparar **autoridades**, nunca prefixos de texto: `github.com.atacante.com` e
/// `https://github.com@atacante.com` têm de dar hosts diferentes de `github.com`. `None` se não for
/// `https://` ou não tiver host.
pub fn https_authority(url: &str) -> Option<(String, u16)> {
    let rest = url.trim().strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Tudo antes do último `@` é usuário/senha: o host de verdade vem depois dele.
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h, p.parse::<u16>().ok()?)
        }
        _ => (hostport, 443),
    };
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    (!host.is_empty() && !host.chars().any(char::is_whitespace)).then_some((host, port))
}

/// O endereço (host e porta) a que a credencial deste provider pertence: o do `base_url`, e
/// `github.com` para o GitHub público (cujo `base_url` fica vazio).
pub fn provider_authority(p: &StoredProvider) -> Option<(String, u16)> {
    let base = p.base_url.trim().trim_end_matches('/');
    if kind_of(p) == GitProviderKind::Github
        && (base.is_empty() || base == "https://github.com" || base == "http://github.com")
    {
        return Some(("github.com".to_string(), 443));
    }
    https_authority(base)
}

/// A credencial deste provider pode ser enviada a `url`? Só se o host (e a porta) forem os do próprio
/// provider. Sem isso, quem escolhe a URL de um clone ou do `state_git` receberia o token do provider:
/// bastava apontar para um servidor que responda "401, preciso de login".
pub fn credential_matches_url(p: &StoredProvider, url: &str) -> bool {
    match (provider_authority(p), https_authority(url)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// [`usable_token`], mas só se `url` for do provider (ver [`credential_matches_url`]).
pub fn usable_token_for_url(
    secrets: &SecretsManager,
    p: &StoredProvider,
    url: &str,
) -> Result<String> {
    if !credential_matches_url(p, url) {
        let onde = provider_authority(p)
            .map(|(h, port)| format!("{h}:{port}"))
            .unwrap_or_else(|| "?".into());
        anyhow::bail!(
            "o endereço não é do provider git '{}' ({onde}): a credencial dele não é enviada a outro servidor",
            p.name
        );
    }
    usable_token(secrets, p)
}

/// Attempts to refresh an expired OAuth access token, persisting the new pair.
/// Returns the fresh access token on success. PAT providers (or those without a
/// refresh token) yield `None` — there's nothing to refresh.
pub async fn refresh_access_token(
    db: &Db,
    secrets: &SecretsManager,
    p: &StoredProvider,
) -> Option<String> {
    if p.auth_mode != shared::GitAuthMode::OAuth.as_str() {
        return None;
    }
    let client_id = p.oauth_client_id.as_deref()?;
    let client_secret = secrets
        .decrypt(p.oauth_client_secret_enc.as_deref()?)
        .ok()?;
    let refresh = secrets.decrypt(p.refresh_token_enc.as_deref()?).ok()?;

    let tokens = match kind_of(p) {
        GitProviderKind::Gitea => {
            let t = gitea::refresh(&p.base_url, client_id, &client_secret, &refresh)
                .await
                .ok()?;
            OAuthTokens {
                access_token: t.access_token,
                refresh_token: t.refresh_token,
            }
        }
        GitProviderKind::Github => {
            let t = github::refresh(&p.base_url, client_id, &client_secret, &refresh)
                .await
                .ok()?;
            OAuthTokens {
                access_token: t.access_token,
                refresh_token: t.refresh_token,
            }
        }
    };

    let access_enc = secrets.encrypt(&tokens.access_token).ok()?;
    let refresh_enc = tokens
        .refresh_token
        .as_deref()
        .and_then(|r| secrets.encrypt(r).ok());
    git_providers::set_tokens(
        db,
        &p.id,
        Some(&access_enc),
        refresh_enc.as_deref(),
        p.account_login.as_deref().unwrap_or_default(),
        p.account_avatar.as_deref(),
    )
    .await
    .ok()?;
    Some(tokens.access_token)
}

#[cfg(test)]
mod host_da_credencial_tests {
    use super::*;

    fn provider(kind: &str, base_url: &str) -> StoredProvider {
        StoredProvider {
            id: "gp_1".into(),
            kind: kind.into(),
            name: "teste".into(),
            base_url: base_url.into(),
            auth_mode: "Pat".into(),
            oauth_client_id: None,
            oauth_client_secret_enc: None,
            access_token_enc: None,
            refresh_token_enc: None,
            account_login: None,
            account_avatar: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn autoridade_ignora_caixa_usuario_e_caminho() {
        let gh = ("github.com".to_string(), 443);
        assert_eq!(
            https_authority("https://github.com/a/b.git"),
            Some(gh.clone())
        );
        assert_eq!(
            https_authority("https://GitHub.COM/a/b.git"),
            Some(gh.clone())
        );
        assert_eq!(
            https_authority("https://user:tok@github.com/a/b.git"),
            Some(gh.clone())
        );
        assert_eq!(
            https_authority("https://git.exemplo.com:8443/x"),
            Some(("git.exemplo.com".into(), 8443))
        );
        assert_eq!(https_authority("http://github.com/a"), None, "só https");
        assert_eq!(https_authority("git@github.com:a/b.git"), None);
        assert_eq!(https_authority("https://"), None);
    }

    #[test]
    fn github_publico_so_aceita_github_com() {
        let p = provider("github", "");
        assert!(credential_matches_url(
            &p,
            "https://github.com/dono/repo.git"
        ));
        assert!(credential_matches_url(
            &p,
            "https://x:y@GITHUB.com/dono/repo.git"
        ));
        // as tentativas de enganar
        assert!(!credential_matches_url(
            &p,
            "https://github.com.atacante.com/dono/repo.git"
        ));
        assert!(!credential_matches_url(
            &p,
            "https://github.com@atacante.com/dono/repo.git"
        ));
        assert!(!credential_matches_url(
            &p,
            "https://atacante.com/github.com/repo.git"
        ));
        assert!(
            !credential_matches_url(&p, "https://github.com:8443/dono/repo.git"),
            "outra porta"
        );
        assert!(
            !credential_matches_url(&p, "http://github.com/dono/repo.git"),
            "sem TLS"
        );
        assert!(
            !credential_matches_url(&p, "https://api.github.com/dono/repo.git"),
            "outro host"
        );
    }

    #[test]
    fn gitea_e_enterprise_usam_o_host_do_base_url() {
        let g = provider("gitea", "https://git.exemplo.com:3000/");
        assert!(credential_matches_url(
            &g,
            "https://git.exemplo.com:3000/o/r.git"
        ));
        assert!(
            !credential_matches_url(&g, "https://git.exemplo.com/o/r.git"),
            "porta diferente"
        );
        assert!(!credential_matches_url(
            &g,
            "https://outro.exemplo.com:3000/o/r.git"
        ));
        let e = provider("github", "https://ghe.empresa.com");
        assert!(credential_matches_url(
            &e,
            "https://ghe.empresa.com/o/r.git"
        ));
        assert!(
            !credential_matches_url(&e, "https://github.com/o/r.git"),
            "Enterprise não é o github.com"
        );
    }

    #[test]
    fn provider_sem_base_url_valido_nao_envia_credencial() {
        let p = provider("gitea", "");
        assert!(!credential_matches_url(&p, "https://qualquer.com/o/r.git"));
    }
}
