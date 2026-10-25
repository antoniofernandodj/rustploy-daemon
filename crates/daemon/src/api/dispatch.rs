//! `dispatch`: o `match` que manda cada `Command` para o seu handler em
//! `handlers/`. Ver `docs/indice/comandos.md`.

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};
use tracing::info;

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    use super::handlers;

    let cmd_name = match &cmd {
        Command::Ping => "Ping",
        Command::DaemonStatus => "DaemonStatus",
        Command::CheckUpdate { .. } => "CheckUpdate",
        Command::ApplyUpdate { .. } => "ApplyUpdate",
        Command::UpdateStatus => "UpdateStatus",
        Command::UpdateRollback => "UpdateRollback",
        Command::DeployEngineStatus => "DeployEngineStatus",
        Command::ProjectCreate { .. } => "ProjectCreate",
        Command::ProjectList => "ProjectList",
        Command::ProjectDelete { .. } => "ProjectDelete",
        Command::ProjectUpdate { .. } => "ProjectUpdate",
        Command::ProjectEnvSet { .. } => "ProjectEnvSet",
        Command::ServiceCreate(_) => "ServiceCreate",
        Command::ServiceExportPlan { .. } => "ServiceExportPlan",
        Command::ServiceExport { .. } => "ServiceExport",
        Command::ServiceImport(_) => "ServiceImport",
        Command::ServiceList { .. } => "ServiceList",
        Command::ServiceGet { .. } => "ServiceGet",
        Command::ServiceUpdate { .. } => "ServiceUpdate",
        Command::ServiceDelete { .. } => "ServiceDelete",
        Command::DeployStart { .. } => "DeployStart",
        Command::DeployAbort { .. } => "DeployAbort",
        Command::DeployDelete { .. } => "DeployDelete",
        Command::DeployHistory { .. } => "DeployHistory",
        Command::DeployRollback { .. } => "DeployRollback",
        Command::ServiceStop { .. } => "ServiceStop",
        Command::ServiceReload { .. } => "ServiceReload",
        Command::LogsGet { .. } => "LogsGet",
        Command::RecentDeployments { .. } => "RecentDeployments",
        Command::GetBuildLogs { .. } => "GetBuildLogs",
        Command::ServiceConnectionInfo { .. } => "ServiceConnectionInfo",
        Command::SharedAccessList { .. } => "SharedAccessList",
        Command::SharedAccessGrant { .. } => "SharedAccessGrant",
        Command::SharedAccessRevoke { .. } => "SharedAccessRevoke",
        Command::ManagedDatabaseList { .. } => "ManagedDatabaseList",
        Command::ManagedDatabaseCreate(_) => "ManagedDatabaseCreate",
        Command::ManagedDatabaseDelete { .. } => "ManagedDatabaseDelete",
        Command::ManagedDatabaseListAll => "ManagedDatabaseListAll",
        Command::MigrationStart(_) => "MigrationStart",
        Command::MigrationGet { .. } => "MigrationGet",
        Command::MigrationList { .. } => "MigrationList",
        Command::MigrationRollback { .. } => "MigrationRollback",
        Command::MigrationDiscard { .. } => "MigrationDiscard",
        Command::GetWebhookUrl { .. } => "GetWebhookUrl",
        Command::RegenerateWebhookToken { .. } => "RegenerateWebhookToken",
        Command::GetDaemonSettings => "GetDaemonSettings",
        Command::SetDaemonSettings { .. } => "SetDaemonSettings",
        Command::SecretSet { .. } => "SecretSet",
        Command::SecretDelete { .. } => "SecretDelete",
        Command::SecretList { .. } => "SecretList",
        Command::ManifestApply { .. } => "ManifestApply",
        Command::ManifestExport { .. } => "ManifestExport",
        Command::ManifestExportAll => "ManifestExportAll",
        Command::ManifestImport { .. } => "ManifestImport",
        Command::JobCreate { .. } => "JobCreate",
        Command::JobUpdate { .. } => "JobUpdate",
        Command::JobDelete { .. } => "JobDelete",
        Command::JobList { .. } => "JobList",
        Command::JobListAll => "JobListAll",
        Command::JobRunNow { .. } => "JobRunNow",
        Command::JobRunCancel { .. } => "JobRunCancel",
        Command::JobRunHistory { .. } => "JobRunHistory",
        Command::GetJobLogs { .. } => "GetJobLogs",
        Command::JobWarm { .. } => "JobWarm",
        Command::JobWarmStatus { .. } => "JobWarmStatus",
        Command::PruneContainers => "PruneContainers",
        Command::PruneVolumes { .. } => "PruneVolumes",
        Command::PruneImages { .. } => "PruneImages",
        Command::PruneBuildCache => "PruneBuildCache",
        Command::PruneNetworks => "PruneNetworks",
        Command::DockerCleanupConfigGet => "DockerCleanupConfigGet",
        Command::DockerCleanupConfigSet { .. } => "DockerCleanupConfigSet",
        Command::DockerCleanupRunNow => "DockerCleanupRunNow",
        Command::ServiceWarmStatusList => "ServiceWarmStatusList",
        Command::ServiceWarm { .. } => "ServiceWarm",
        Command::DockerImages => "DockerImages",
        Command::DockerVolumes => "DockerVolumes",
        Command::DockerNetworks => "DockerNetworks",
        Command::DockerContainers => "DockerContainers",
        Command::RemoveContainer { .. } => "RemoveContainer",
        Command::RemoveImage { .. } => "RemoveImage",
        Command::RemoveVolume { .. } => "RemoveVolume",
        Command::RemoveNetwork { .. } => "RemoveNetwork",
        Command::StopAllManaged => "StopAllManaged",
        Command::IngressRoutes => "IngressRoutes",
        Command::IngressReconcile { .. } => "IngressReconcile",
        Command::EnvBackupList => "EnvBackupList",
        Command::EnvBackupRestore { .. } => "EnvBackupRestore",
        Command::GitProviderList => "GitProviderList",
        Command::GitProviderCreate { .. } => "GitProviderCreate",
        Command::GitProviderDelete { .. } => "GitProviderDelete",
        Command::GitOAuthStart { .. } => "GitOAuthStart",
        Command::GitRepoList { .. } => "GitRepoList",
        Command::GitBranchList { .. } => "GitBranchList",
        Command::WizardCatalog { .. } => "WizardCatalog",
        Command::WizardCreate(_) => "WizardCreate",
        Command::Snapshot => "Snapshot",
        Command::RegistryStatus => "RegistryStatus",
        Command::RegistryRepoList => "RegistryRepoList",
        Command::RegistryTagList { .. } => "RegistryTagList",
        Command::RegistryTagDelete { .. } => "RegistryTagDelete",
        Command::RegistryRepoDelete { .. } => "RegistryRepoDelete",
        Command::RegistryGc => "RegistryGc",
        Command::RegistryTokenCreate { .. } => "RegistryTokenCreate",
        Command::RegistryTokenList => "RegistryTokenList",
        Command::RegistryTokenRevoke { .. } => "RegistryTokenRevoke",
        _ => "Unknown",
    };
    info!(command = cmd_name, "→ Request");

    let resp = match &cmd {
        Command::SharedAccessList { .. }
        | Command::SharedAccessGrant { .. }
        | Command::SharedAccessRevoke { .. }
        | Command::ManagedDatabaseList { .. }
        | Command::ManagedDatabaseCreate(_)
        | Command::ManagedDatabaseDelete { .. }
        | Command::ManagedDatabaseListAll
        | Command::MigrationStart(_)
        | Command::MigrationGet { .. }
        | Command::MigrationList { .. }
        | Command::MigrationRollback { .. }
        | Command::MigrationDiscard { .. } => handlers::databases::dispatch(state, cmd).await,
        Command::DeployEngineStatus
        | Command::DeployStart { .. }
        | Command::DeployAbort { .. }
        | Command::DeployQueuePromote { .. }
        | Command::DeployQueueReorder { .. }
        | Command::DeployQueuePause { .. }
        | Command::DeployDelete { .. }
        | Command::DeployHistory { .. }
        | Command::DeployRollback { .. }
        | Command::RecentDeployments { .. }
        | Command::GetBuildLogs { .. } => handlers::deploys::dispatch(state, cmd).await,
        Command::PruneContainers
        | Command::PruneVolumes { .. }
        | Command::PruneImages { .. }
        | Command::PruneBuildCache
        | Command::PruneNetworks
        | Command::DockerCleanupConfigGet
        | Command::DockerCleanupConfigSet { .. }
        | Command::DockerCleanupRunNow
        | Command::ServiceWarmStatusList
        | Command::ServiceWarm { .. }
        | Command::DockerImages
        | Command::DockerVolumes
        | Command::DockerNetworks
        | Command::DockerContainers
        | Command::RemoveContainer { .. }
        | Command::RemoveImage { .. }
        | Command::RemoveVolume { .. }
        | Command::RemoveNetwork { .. }
        | Command::StopAllManaged => handlers::docker::dispatch(state, cmd).await,
        Command::GitProviderList
        | Command::GitProviderCreate { .. }
        | Command::GitProviderDelete { .. }
        | Command::GitOAuthStart { .. }
        | Command::GitRepoList { .. }
        | Command::GitBranchList { .. } => handlers::git::dispatch(state, cmd).await,
        Command::ServiceExportPlan { .. }
        | Command::ServiceExport { .. }
        | Command::ServiceImport(_)
        | Command::ManifestApply { .. }
        | Command::ManifestExport { .. }
        | Command::ManifestExportAll
        | Command::ManifestImport { .. } => handlers::iac::dispatch(state, cmd).await,
        Command::JobCreate { .. }
        | Command::JobUpdate { .. }
        | Command::JobDelete { .. }
        | Command::JobList { .. }
        | Command::JobListAll
        | Command::JobRunNow { .. }
        | Command::JobRunCancel { .. }
        | Command::JobRunHistory { .. }
        | Command::GetJobLogs { .. }
        | Command::JobWarmStatus { .. }
        | Command::JobWarm { .. } => handlers::jobs::dispatch(state, cmd).await,
        Command::Ping
        | Command::DaemonStatus
        | Command::CheckUpdate { .. }
        | Command::ApplyUpdate { .. }
        | Command::UpdateStatus
        | Command::UpdateRollback
        | Command::IngressRoutes
        | Command::IngressReconcile { .. }
        | Command::LogsGet { .. } => handlers::observability::dispatch(state, cmd).await,
        Command::ProjectCreate { .. }
        | Command::ProjectList
        | Command::ProjectDelete { .. }
        | Command::ProjectUpdate { .. }
        | Command::ProjectEnvSet { .. } => handlers::projects::dispatch(state, cmd).await,
        Command::RegistryStatus
        | Command::RegistryRepoList
        | Command::RegistryTagList { .. }
        | Command::RegistryTagDelete { .. }
        | Command::RegistryRepoDelete { .. }
        | Command::RegistryGc
        | Command::RegistryTokenCreate { .. }
        | Command::RegistryTokenList
        | Command::RegistryTokenRevoke { .. } => handlers::registry::dispatch(state, cmd).await,
        Command::SecretSet { .. } | Command::SecretDelete { .. } | Command::SecretList { .. } => {
            handlers::secrets::dispatch(state, cmd).await
        }
        Command::ServiceCreate(_)
        | Command::ServiceList { .. }
        | Command::ServiceConnectionInfo { .. }
        | Command::ServiceGet { .. }
        | Command::ServiceUpdate { .. }
        | Command::ServiceDelete { .. }
        | Command::ServiceStop { .. }
        | Command::ServiceReload { .. }
        | Command::GetWebhookUrl { .. }
        | Command::RegenerateWebhookToken { .. }
        | Command::WizardCatalog { .. }
        | Command::WizardCreate(_) => handlers::services::dispatch(state, cmd).await,
        Command::EnvBackupList
        | Command::EnvBackupRestore { .. }
        | Command::GetDaemonSettings
        | Command::SetDaemonSettings { .. } => handlers::settings::dispatch(state, cmd).await,
        // `snapshot` chama `dispatch` internamente (fan-out de DaemonStatus/
        // ProjectList/…), então boxamos o future para quebrar a recursão async.
        Command::Snapshot => {
            RpResponse::Snapshot(Box::pin(crate::api::http::snapshot(&state)).await)
        }
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    };

    let ok = !matches!(resp, RpResponse::Err { .. });
    info!(ok, command = cmd_name, "← Response");
    resp
}
