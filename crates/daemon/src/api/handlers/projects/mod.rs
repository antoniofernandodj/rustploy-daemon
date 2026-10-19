//! Handlers de projects: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod project_create;
pub mod project_delete;
pub mod project_env_set;
pub mod project_list;
pub mod project_update;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::ProjectCreate { name, description } => {
            project_create::handle(state, name, description).await
        }
        Command::ProjectList => project_list::handle(state).await,
        Command::ProjectDelete { id } => project_delete::handle(state, id).await,
        Command::ProjectUpdate {
            id,
            name,
            description,
        } => project_update::handle(state, id, name, description).await,
        Command::ProjectEnvSet {
            project_id,
            env_vars,
            env_comments,
        } => project_env_set::handle(state, project_id, env_vars, env_comments).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
