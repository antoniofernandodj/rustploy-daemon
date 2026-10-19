//! Handlers de git: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod git_branch_list;
pub mod git_oauth_start;
pub mod git_provider_create;
pub mod git_provider_delete;
pub mod git_provider_list;
pub mod git_repo_list;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::GitProviderList => git_provider_list::handle(state).await,
        Command::GitProviderCreate {
            kind,
            name,
            base_url,
            auth_mode,
            oauth_client_id,
            oauth_client_secret,
            pat,
        } => {
            git_provider_create::handle(
                state,
                kind,
                name,
                base_url,
                auth_mode,
                oauth_client_id,
                oauth_client_secret,
                pat,
            )
            .await
        }
        Command::GitProviderDelete { id } => git_provider_delete::handle(state, id).await,
        Command::GitOAuthStart { provider_id } => git_oauth_start::handle(state, provider_id).await,
        Command::GitRepoList { provider_id } => git_repo_list::handle(state, provider_id).await,
        Command::GitBranchList {
            provider_id,
            repo_full_name,
        } => git_branch_list::handle(state, provider_id, repo_full_name).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
