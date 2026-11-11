//! Handlers `Command::ServerRecover*`: recuperar um servidor limpo (git + chave de recuperação + S3).
//! Não estão disponíveis ao agente MCP: levam a chave de recuperação e um token.

use crate::bootstrap::state::AppState;
use crate::features::data_backup::recover;
use shared::{GitAuthMode, GitProviderKind, Response};

/// `https://host[:porta]/…` → `(github|gitea, base_url)`. O github.com usa a base padrão.
fn provider_for(remote: &str) -> (GitProviderKind, String) {
    let rest = remote
        .trim()
        .strip_prefix("https://")
        .unwrap_or(remote.trim());
    let authority = rest.split('/').next().unwrap_or("");
    if authority.eq_ignore_ascii_case("github.com") {
        (GitProviderKind::Github, String::new())
    } else {
        (GitProviderKind::Gitea, format!("https://{authority}"))
    }
}

pub async fn start(
    state: AppState,
    remote: String,
    token: String,
    branch: Option<String>,
    recovery_key: String,
) -> Response {
    let remote = remote.trim().to_string();
    let local = remote.starts_with("file://") || remote.starts_with('/');
    if !local && !remote.starts_with("https://") {
        return Response::err(
            "InvalidInput",
            "informe o endereço do repositório: https://…, file://… ou um caminho absoluto",
        );
    }
    // https: o token vira um provider git deste servidor (a chamada valida o token no provedor).
    // Remoto local não tem credencial.
    let provider_id = if local {
        None
    } else {
        if token.trim().is_empty() {
            return Response::err("InvalidInput", "informe o token de acesso ao repositório");
        }
        let (kind, base_url) = provider_for(&remote);
        let created = crate::api::handlers::git::git_provider_create::handle(
            state.clone(),
            kind,
            "Recuperação".into(),
            base_url,
            GitAuthMode::Pat,
            None,
            None,
            Some(token),
        )
        .await;
        match created {
            Response::GitProviderInfo(p) => Some(p.id.clone()),
            other => return other, // o erro do provedor (token inválido…) sobe como veio
        }
    };
    match recover::start(
        state.deploy_ctx(),
        state.state_git.clone(),
        remote,
        provider_id,
        branch,
        recovery_key,
    )
    .await
    {
        Ok(info) => Response::ServerRecover(Box::new(info)),
        Err(e) => Response::err("RecoverFailed", format!("{e:#}")),
    }
}

pub async fn status() -> Response {
    Response::ServerRecover(Box::new(recover::status()))
}

pub async fn run(state: AppState) -> Response {
    match recover::run(state.deploy_ctx()).await {
        Ok(()) => Response::Ok,
        Err(e) => Response::err("RecoverNotStarted", format!("{e:#}")),
    }
}

pub async fn cancel(state: AppState) -> Response {
    Response::ServerRecover(Box::new(recover::cancel(&state.deploy_ctx())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_usa_a_base_padrao_e_os_outros_viram_gitea() {
        assert_eq!(
            provider_for("https://github.com/org/estado.git"),
            (GitProviderKind::Github, String::new())
        );
        assert_eq!(
            provider_for("https://GitHub.com/org/x.git").0,
            GitProviderKind::Github
        );
        assert_eq!(
            provider_for("https://gitea.chiquitos.tech:3000/org/x.git"),
            (
                GitProviderKind::Gitea,
                "https://gitea.chiquitos.tech:3000".to_string()
            )
        );
        // `github.com.evil.com` não é o GitHub
        assert_eq!(
            provider_for("https://github.com.evil.com/x.git").0,
            GitProviderKind::Gitea
        );
    }
}
