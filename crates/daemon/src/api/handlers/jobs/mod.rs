//! Handlers de jobs: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod get_job_logs;
pub mod job_create;
pub mod job_delete;
pub mod job_list;
pub mod job_list_all;
pub mod job_run_cancel;
pub mod job_run_history;
pub mod job_run_now;
pub mod job_update;
pub mod job_warm;
pub mod job_warm_status;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::JobCreate {
            project_id,
            trigger_service_id,
            name,
            compose,
            git_source,
            main_service,
            env_vars,
            env_comments,
            recurrence,
        } => {
            job_create::handle(
                state,
                project_id,
                trigger_service_id,
                name,
                compose,
                git_source,
                main_service,
                env_vars,
                env_comments,
                recurrence,
            )
            .await
        }
        Command::JobUpdate {
            id,
            name,
            compose,
            git_source,
            main_service,
            env_vars,
            env_comments,
            enabled,
            recurrence,
        } => {
            job_update::handle(
                state,
                id,
                name,
                compose,
                git_source,
                main_service,
                env_vars,
                env_comments,
                enabled,
                recurrence,
            )
            .await
        }
        Command::JobDelete { id } => job_delete::handle(state, id).await,
        Command::JobList { project_id } => job_list::handle(state, project_id).await,
        Command::JobListAll => job_list_all::handle(state).await,
        Command::JobRunNow { id } => job_run_now::handle(state, id).await,
        Command::JobRunCancel { job_run_id } => job_run_cancel::handle(state, job_run_id).await,
        Command::JobRunHistory { job_id, limit } => {
            job_run_history::handle(state, job_id, limit).await
        }
        Command::GetJobLogs { job_run_id } => get_job_logs::handle(state, job_run_id).await,
        Command::JobWarmStatus { job_id } => job_warm_status::handle(state, job_id).await,
        Command::JobWarm { job_id } => job_warm::handle(state, job_id).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
