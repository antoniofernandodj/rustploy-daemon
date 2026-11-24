//! Handlers de projects: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod environments;
pub mod project_create;
pub mod project_delete;
pub mod project_env_set;
pub mod project_list;
pub mod project_services_reorder;
pub mod project_update;

use crate::bootstrap::state::AppState;
use crate::protocol::{Command, Response as RpResponse};

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
        Command::ProjectServicesReorder {
            project_id,
            service_ids,
        } => project_services_reorder::handle(state, project_id, service_ids).await,
        Command::ProjectEnvSet {
            project_id,
            env_vars,
            env_comments,
        } => project_env_set::handle(state, project_id, env_vars, env_comments).await,
        Command::EnvironmentCreate { project_id, name } => {
            environments::create(state, project_id, name).await
        }
        Command::EnvironmentList { project_id } => environments::list(state, project_id),
        Command::EnvironmentCompare { project_id } => {
            environments::compare(state, project_id).await
        }
        Command::EnvironmentPromote {
            from_service_id,
            to_service_id,
        } => environments::promote(state, from_service_id, to_service_id).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
