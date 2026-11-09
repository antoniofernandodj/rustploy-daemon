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
        Command::ProjectServicesReorder { .. } => "ProjectServicesReorder",
        Command::ProjectEnvSet { .. } => "ProjectEnvSet",
        Command::ServiceCreate(_) => "ServiceCreate",
        Command::ServiceExportPlan { .. } => "ServiceExportPlan",
        Command::ServiceExport { .. } => "ServiceExport",
        Command::ServiceDataPlan { .. } => "ServiceDataPlan",
        Command::ServiceImport(_) => "ServiceImport",
        Command::ServiceList { .. } => "ServiceList",
        Command::ServiceGet { .. } => "ServiceGet",
        Command::ServiceUpdate { .. } => "ServiceUpdate",
        Command::ServiceDelete { .. } => "ServiceDelete",
        Command::DeployStart { .. } => "DeployStart",
        Command::DeployWithDependencies { .. } => "DeployWithDependencies",
        Command::ProjectDeploy { .. } => "ProjectDeploy",
        Command::DeployWithDependenciesPlan { .. } => "DeployWithDependenciesPlan",
        Command::ProjectDeployPlan { .. } => "ProjectDeployPlan",
        Command::DeployAbort { .. } => "DeployAbort",
        Command::DeployDelete { .. } => "DeployDelete",
        Command::DeployHistory { .. } => "DeployHistory",
        Command::DeployRollback { .. } => "DeployRollback",
        Command::ServiceStop { .. } => "ServiceStop",
        Command::ServiceReload { .. } => "ServiceReload",
        Command::LogsGet { .. } => "LogsGet",
        Command::LogsHistory { .. } => "LogsHistory",
        Command::LogsExecutions { .. } => "LogsExecutions",
        Command::LogsExecutionDelete { .. } => "LogsExecutionDelete",
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
        Command::SecretReveal { .. } => "SecretReveal",
        Command::InfraAudit => "InfraAudit",
        Command::InfraDownload => "InfraDownload",
        Command::StateGitStatus => "StateGitStatus",
        Command::StateGitConfigure { .. } => "StateGitConfigure",
        Command::StateGitSyncNow => "StateGitSyncNow",
        Command::StateGitResume => "StateGitResume",
        Command::StateGitResolve { .. } => "StateGitResolve",
        Command::StateGitDismissRemoval { .. } => "StateGitDismissRemoval",
        Command::InfraAuditAllow { .. } => "InfraAuditAllow",
        Command::SecretMove { .. } => "SecretMove",
        Command::SecretRename { .. } => "SecretRename",
        Command::EnvKeyRename { .. } => "EnvKeyRename",
        Command::EnvValueSet { .. } => "EnvValueSet",
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
        Command::StopContainer { .. } => "StopContainer",
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
        Command::DeployQueueReorder { .. } => "DeployQueueReorder",
        Command::DeployQueuePromote { .. } => "DeployQueuePromote",
        Command::DeployQueuePause { .. } => "DeployQueuePause",
        Command::LogsSubscribe { .. } => "LogsSubscribe",
        Command::LogsUnsubscribe { .. } => "LogsUnsubscribe",
        Command::MetricsSubscribe { .. } => "MetricsSubscribe",
        Command::MetricsUnsubscribe { .. } => "MetricsUnsubscribe",
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
        | Command::DeployWithDependencies { .. }
        | Command::ProjectDeploy { .. }
        | Command::DeployWithDependenciesPlan { .. }
        | Command::ProjectDeployPlan { .. }
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
        | Command::StopContainer { .. }
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
        | Command::ServiceImport(_) => handlers::iac::dispatch(state, cmd).await,
        Command::ServiceDataPlan { service_id } => {
            crate::features::service_data::plan(state.deploy_ctx(), service_id.clone()).await
        }
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
        | Command::LogsGet { .. }
        | Command::LogsHistory { .. }
        | Command::LogsExecutions { .. }
        | Command::LogsExecutionDelete { .. } => {
            handlers::observability::dispatch(state, cmd).await
        }
        Command::ProjectCreate { .. }
        | Command::ProjectList
        | Command::ProjectDelete { .. }
        | Command::ProjectUpdate { .. }
        | Command::ProjectServicesReorder { .. }
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
        Command::SecretSet { .. }
        | Command::SecretDelete { .. }
        | Command::SecretList { .. }
        | Command::SecretReveal { .. }
        | Command::SecretMove { .. }
        | Command::SecretRename { .. }
        | Command::EnvKeyRename { .. }
        | Command::EnvValueSet { .. }
        | Command::InfraAudit
        | Command::InfraDownload => handlers::secrets::dispatch(state, cmd).await,
        Command::StateGitStatus
        | Command::StateGitConfigure { .. }
        | Command::StateGitSyncNow
        | Command::StateGitResume
        | Command::StateGitResolve { .. }
        | Command::StateGitDismissRemoval { .. }
        | Command::InfraAuditAllow { .. } => handlers::state_git::dispatch(state, cmd).await,
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

/// O manual do agente (`agent/manual.md`) é escrito à mão e já divergiu dos `Command` reais.
/// Os nomes vêm do `match` de nomes de [`dispatch`], que **não tem braço curinga** (`_`): o compilador
/// obriga cada `Command` novo a entrar nele — uma versão anterior tinha um `_ => "Unknown"` que deixava
/// 7 comandos de fora sem ninguém ver. Cobrar cada nome no manual fecha a lacuna sem `schemars` nem
/// publicar o `shared`: um comando novo que ninguém documentou **quebra este teste**.
#[cfg(test)]
mod manual_do_agente {
    /// Comandos que o manual não precisa listar, cada um com a razão.
    const FORA_DO_MANUAL: &[(&str, &str)] = &[
        (
            "GitOAuthStart",
            "devolve uma URL para uma pessoa abrir no navegador; o agente usa PAT (`GitProviderCreate`)",
        ),
        // Restos do protocolo antigo, sem handler: o streaming é por SSE (`/api/events`, `/api/services/<id>/logs`).
        ("LogsSubscribe", "sem handler; use o SSE de logs"),
        ("LogsUnsubscribe", "sem handler; use o SSE de logs"),
        (
            "MetricsSubscribe",
            "sem handler; as métricas vêm no SSE `/api/events`",
        ),
        (
            "MetricsUnsubscribe",
            "sem handler; as métricas vêm no SSE `/api/events`",
        ),
    ];

    fn nomes_dos_comandos() -> Vec<String> {
        let fonte = include_str!("dispatch.rs");
        let corpo = fonte
            .split("let cmd_name = match &cmd {")
            .nth(1)
            .and_then(|r| r.split("};").next())
            .expect("o match de nomes de comandos sumiu do dispatch");
        corpo
            .lines()
            .filter_map(|l| l.split("=> \"").nth(1))
            .filter_map(|r| r.split('"').next())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn todo_comando_esta_no_manual_do_agente() {
        let manual = include_str!("agent/manual.md");
        let nomes = nomes_dos_comandos();
        assert!(nomes.len() > 100, "extraí só {} nomes", nomes.len());
        let ausentes: Vec<&String> = nomes
            .iter()
            .filter(|n| !FORA_DO_MANUAL.iter().any(|(f, _)| f == n))
            .filter(|n| {
                // palavra inteira: `ServiceGet` não pode ser "achado" dentro de `ServiceGetX`
                !manual.match_indices(n.as_str()).any(|(i, _)| {
                    let antes = manual[..i].chars().next_back();
                    let depois = manual[i + n.len()..].chars().next();
                    !antes.is_some_and(|c| c.is_alphanumeric() || c == '_')
                        && !depois.is_some_and(|c| c.is_alphanumeric() || c == '_')
                })
            })
            .collect();
        assert!(
            ausentes.is_empty(),
            "comandos que o manual do agente não menciona (documente em agent/manual.md ou \
             liste em FORA_DO_MANUAL com a razão): {ausentes:?}"
        );
    }
}
