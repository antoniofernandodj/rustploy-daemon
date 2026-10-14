//! Checkout do repositório de um job git-sourced, compartilhado pela execução
//! (`runner`) e pelo aquecimento (`warm`): a mesma resolução de credenciais do
//! deploy de serviço git-sourced.

use crate::db::Db;
use crate::deploy::git::{self, CloneOptions, CloneProgress};
use crate::secrets::SecretsManager;
use anyhow::Result;
use shared::{Job, JobGitSource};
use std::path::Path;

/// Clona `git` para dentro de `build_dir` (vira o checkout do job).
pub(crate) async fn clone_git_source(
    db: &Db,
    secrets: &SecretsManager,
    job: &Job,
    git: &JobGitSource,
    build_dir: &Path,
    on_progress: impl FnMut(CloneProgress),
) -> Result<()> {
    let (token, username) = git::resolve_clone_credentials(
        db,
        secrets,
        git.provider_id.as_deref(),
        git.credentials.as_deref(),
        git.username.as_deref(),
        &job.project_id,
    )
    .await;

    git::clone(
        CloneOptions {
            url: &git.url,
            branch: &git.branch,
            token: token.as_deref(),
            username: username.as_deref(),
            dir: build_dir,
        },
        on_progress,
    )
    .await
}
