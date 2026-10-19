//! Handlers de registry: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod registry;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::RegistryStatus => registry::status(state).await,
        Command::RegistryRepoList => registry::repo_list(state).await,
        Command::RegistryTagList { repo } => registry::tag_list(state, repo).await,
        Command::RegistryTagDelete { repo, tag } => registry::tag_delete(state, repo, tag).await,
        Command::RegistryRepoDelete { repo } => registry::repo_delete(state, repo).await,
        Command::RegistryGc => registry::gc(state).await,
        Command::RegistryTokenCreate { name, scope } => {
            registry::token_create(state, name, scope).await
        }
        Command::RegistryTokenList => registry::token_list(state).await,
        Command::RegistryTokenRevoke { name } => registry::token_revoke(state, name).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
