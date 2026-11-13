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
        match create_recovery_provider(&state, &remote, token.clone()).await {
            Ok(id) => Some(id),
            Err(resp) => return resp, // o erro do provedor (token inválido…) sobe como veio
        }
    };
    let (kept_remote, kept_branch) = (remote.clone(), branch.clone());
    match recover::start(
        state.deploy_ctx(),
        state.state_git.clone(),
        remote,
        provider_id.clone(),
        branch,
        recovery_key,
    )
    .await
    {
        Ok(info) => {
            if let Some(pid) = provider_id {
                keep_sync_credential(&state, &pid, token, kept_remote, kept_branch).await;
            }
            Response::ServerRecover(Box::new(info))
        }
        Err(e) => Response::err("RecoverFailed", format!("{e:#}")),
    }
}

/// Cria a conta Git "Recuperação" com o token informado (a chamada valida o token no provedor).
async fn create_recovery_provider(
    state: &AppState,
    remote: &str,
    token: String,
) -> Result<String, Response> {
    let (kind, base_url) = provider_for(remote);
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
        Response::GitProviderInfo(p) => Ok(p.id.clone()),
        other => Err(other),
    }
}

/// Restaurar a infra do git **substitui** os providers pelos que estão no repositório, e leva embora a conta
/// "Recuperação" que o sync usava (o token de login não vai ao git). Sem isto o sync fica apontado para um
/// provider que não existe mais: todo fetch/push falha (`provider … não encontrado`) e as mudanças do servidor
/// recuperado nunca chegam ao git. Se a conta sumiu, recria com o mesmo token e reaponta o sync.
async fn keep_sync_credential(
    state: &AppState,
    provider_id: &str,
    token: String,
    remote: String,
    branch: Option<String>,
) {
    let still_there = matches!(
        crate::store::git_providers::get(&state.db, provider_id).await,
        Ok(Some(_))
    );
    if still_there {
        return;
    }
    tracing::warn!(
        provider_id,
        "recuperação: a conta Git usada pelo sync sumiu na restauração da infra; recriando"
    );
    let new_id = match create_recovery_provider(state, &remote, token).await {
        Ok(id) => id,
        Err(_) => {
            tracing::warn!("recuperação: não consegui recriar a conta Git do sync");
            return;
        }
    };
    if let Err(e) = state
        .state_git
        .configure("sync", Some(remote), branch, Some(new_id))
        .await
    {
        tracing::warn!(error = %e, "recuperação: não consegui reapontar o sync para a conta recriada");
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
