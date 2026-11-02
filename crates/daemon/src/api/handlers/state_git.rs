//! Handlers da sincronização do estado de infra com o git (Fase 6 do plano de estado em git).

use crate::bootstrap::state::AppState;
use crate::features::state_git::StateGitError;
use shared::{Command, Response as RpResponse};

fn erro(e: StateGitError) -> RpResponse {
    match e {
        StateGitError::Plaintext(_) => RpResponse::err("PlaintextFound", e.to_string()),
        StateGitError::Invalid(m) => RpResponse::err("InvalidInput", m),
        StateGitError::Other(e) => RpResponse::err("StateGitFailed", format!("{e:#}")),
    }
}

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::StateGitStatus => match state.state_git.status().await {
            Ok(s) => RpResponse::StateGit(s),
            Err(e) => RpResponse::err("StateGitFailed", format!("{e:#}")),
        },
        Command::StateGitConfigure {
            mode,
            remote,
            branch,
            provider_id,
        } => match state
            .state_git
            .configure(&mode, remote, branch, provider_id)
            .await
        {
            Ok(s) => RpResponse::StateGit(s),
            Err(e) => erro(e),
        },
        Command::StateGitSyncNow => match state.state_git.sync_now().await {
            Ok(s) => RpResponse::StateGit(s),
            Err(e) => erro(e),
        },
        Command::InfraAuditAllow {
            project,
            scope,
            what,
            allowed,
        } => match state
            .state_git
            .set_allowed(&project, &scope, &what, allowed)
            .await
        {
            Ok(()) => RpResponse::Ok,
            Err(e) => erro(e),
        },
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
