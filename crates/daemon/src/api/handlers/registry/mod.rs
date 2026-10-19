//! Handlers de registry: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod commands;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::RegistryStatus => commands::status(state).await,
        Command::RegistryRepoList => commands::repo_list(state).await,
        Command::RegistryTagList { repo } => commands::tag_list(state, repo).await,
        Command::RegistryTagDelete { repo, tag } => commands::tag_delete(state, repo, tag).await,
        Command::RegistryRepoDelete { repo } => commands::repo_delete(state, repo).await,
        Command::RegistryGc => commands::gc(state).await,
        Command::RegistryTokenCreate { name, scope } => {
            commands::token_create(state, name, scope).await
        }
        Command::RegistryTokenList => commands::token_list(state).await,
        Command::RegistryTokenRevoke { name } => commands::token_revoke(state, name).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
