//! Handlers de secrets: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod env_key_rename;
pub mod env_value_set;
pub mod infra_audit;
pub mod infra_download;
pub mod secret_delete;
pub mod secret_list;
pub mod secret_move;
pub mod secret_rename;
pub mod secret_reveal;
pub mod secret_set;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::SecretSet {
            project_id,
            service_id,
            job_id,
            name,
            value,
        } => secret_set::handle(state, project_id, service_id, job_id, name, value).await,
        Command::SecretDelete {
            project_id,
            service_id,
            job_id,
            name,
        } => secret_delete::handle(state, project_id, service_id, job_id, name).await,
        Command::SecretList {
            project_id,
            service_id,
            job_id,
        } => secret_list::handle(state, project_id, service_id, job_id).await,
        Command::SecretReveal {
            project_id,
            service_id,
            job_id,
            name,
        } => secret_reveal::handle(state, project_id, service_id, job_id, name).await,
        Command::SecretMove {
            project_id,
            service_id,
            job_id,
            name,
            op,
        } => secret_move::handle(state, project_id, service_id, job_id, name, op).await,
        Command::SecretRename {
            project_id,
            service_id,
            job_id,
            from,
            to,
        } => secret_rename::handle(state, project_id, service_id, job_id, from, to).await,
        Command::EnvKeyRename {
            project_id,
            service_id,
            job_id,
            from,
            to,
        } => env_key_rename::handle(state, project_id, service_id, job_id, from, to).await,
        Command::EnvValueSet {
            project_id,
            service_id,
            job_id,
            key,
            value,
        } => env_value_set::handle(state, project_id, service_id, job_id, key, value).await,
        Command::InfraAudit => infra_audit::handle(state).await,
        Command::InfraDownload => infra_download::handle(state).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
