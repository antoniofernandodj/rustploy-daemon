//! `Command::JobUpdate`: edita um job (compose ou fonte git, serviço principal,
//! env, recorrência, ligado/desligado).

use crate::bootstrap::state::AppState;
use shared::{EnvComment, EnvVar, JobGitSource, Recurrence, Response as RpResponse};

#[allow(clippy::too_many_arguments)]
pub async fn handle(
    state: AppState,
    id: String,
    name: String,
    compose: String,
    git_source: Option<JobGitSource>,
    main_service: String,
    env_vars: Vec<EnvVar>,
    env_comments: Vec<EnvComment>,
    enabled: bool,
    recurrence: Option<Recurrence>,
) -> RpResponse {
    if let Some(git) = &git_source
        && (git.url.trim().is_empty() || git.branch.trim().is_empty())
    {
        return RpResponse::err(
            "InvalidInput",
            "URL e branch do repositório são obrigatórios",
        );
    }

    if let Some(reason) = crate::platform::env_validation::validate(&env_vars) {
        return RpResponse::err("InvalidInput", reason);
    }

    match crate::store::job::update(
        &state.db,
        &id,
        &name,
        &compose,
        git_source.as_ref(),
        &main_service,
        &env_vars,
        &env_comments,
        enabled,
        recurrence,
    )
    .await
    {
        Ok(Some(job)) => RpResponse::Job(Box::new(job)),
        Ok(None) => RpResponse::err("NotFound", "job not found"),
        Err(e) => RpResponse::err(
            "DatabaseError",
            crate::platform::errors::humanize_db_error(&e, "job"),
        ),
    }
}
