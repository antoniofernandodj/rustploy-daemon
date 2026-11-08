//! Níveis da chave de **visitante** (docs/plano-limites-do-agente-mcp.md, itens 0 e D).
//!
//! Com uma chave **mestra** configurada (`[api].master_token`), a chave atual (`[api].token`) vira a de
//! visitante: ela passa por estas regras em **qualquer rota** (`/mcp` e `/api/rpc`), e o `visitor_level`
//! diz até onde ela vai. Sem mestra, nada disto vale e o comportamento é o de sempre.
//!
//! - [`Level::Observer`]: só lê (listas, logs, estado, auditoria). Nada que altere.
//! - [`Level::Operator`]: o dia a dia sobre o que **já existe**: deploy, parar/reiniciar, variáveis,
//!   secrets, domínios, réplicas. Não muda o que roda (fonte, imagem, comando, volumes, compose).
//! - [`Level::Admin`]: tudo o que sobra (criar/apagar projeto e serviço, jobs, docker, providers…),
//!   ainda com a lista de proibições do agente (`tools::proibido_ao_agente`).
//!
//! [`required_level`] é um `match` **exaustivo** (sem `_`): um `Command` novo não compila até alguém
//! decidir o nível dele.

use serde_json::Value;
use shared::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Observer,
    Operator,
    Admin,
}

impl Level {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observer" => Some(Self::Observer),
            "operator" => Some(Self::Operator),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }
}

/// Quem está chamando. `Master` (ou a chave única de sempre, sem mestra configurada) não passa por nível.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    Master,
    Visitor(Level),
}

impl Caller {
    /// O nível a impor, ou `None` para quem não tem limite de nível.
    pub fn level(self) -> Option<Level> {
        match self {
            Self::Master => None,
            Self::Visitor(l) => Some(l),
        }
    }
}

tokio::task_local! {
    /// O nível do visitante da requisição em andamento (`None` = sem limite). Definido na borda HTTP
    /// (`api::http`) em volta de `/mcp` e do `/api/rpc` de um visitante; lido por `tools::rpc`.
    pub static VISITOR_LEVEL: Option<Level>;
}

/// O nível a impor agora, vindo do contexto da requisição.
pub fn current_level() -> Option<Level> {
    VISITOR_LEVEL.try_with(|l| *l).ok().flatten()
}

/// O nível mínimo para executar o comando.
pub fn required_level(cmd: &Command) -> Level {
    match cmd {
        Command::Ping
        | Command::DaemonStatus
        | Command::CheckUpdate { .. }
        | Command::UpdateStatus
        | Command::DeployEngineStatus
        | Command::ProjectList
        | Command::ServiceExportPlan { .. }
        | Command::ServiceList { .. }
        | Command::ServiceGet { .. }
        | Command::DeployHistory { .. }
        | Command::LogsGet { .. }
        | Command::LogsHistory { .. }
        | Command::LogsExecutions { .. }
        | Command::RecentDeployments { .. }
        | Command::GetBuildLogs { .. }
        | Command::ServiceConnectionInfo { .. }
        | Command::SharedAccessList { .. }
        | Command::ManagedDatabaseList { .. }
        | Command::ManagedDatabaseListAll
        | Command::MigrationGet { .. }
        | Command::MigrationList { .. }
        | Command::GetDaemonSettings
        | Command::SecretList { .. }
        | Command::InfraAudit
        | Command::InfraDownload
        | Command::StateGitStatus
        | Command::JobList { .. }
        | Command::JobListAll
        | Command::JobRunHistory { .. }
        | Command::GetJobLogs { .. }
        | Command::JobWarmStatus { .. }
        | Command::DockerCleanupConfigGet
        | Command::ServiceWarmStatusList
        | Command::DockerImages
        | Command::DockerVolumes
        | Command::DockerNetworks
        | Command::DockerContainers
        | Command::IngressRoutes
        | Command::EnvBackupList
        | Command::GitProviderList
        | Command::GitRepoList { .. }
        | Command::GitBranchList { .. }
        | Command::WizardCatalog { .. }
        | Command::Snapshot
        | Command::RegistryStatus
        | Command::RegistryRepoList
        | Command::RegistryTagList { .. }
        | Command::RegistryTokenList
        | Command::LogsSubscribe { .. }
        | Command::LogsUnsubscribe { .. }
        | Command::MetricsSubscribe { .. }
        | Command::MetricsUnsubscribe { .. }
        | Command::DeployWithDependenciesPlan { .. }
        | Command::ProjectDeployPlan { .. } => Level::Observer,
        Command::DeployStart { .. }
        | Command::DeployWithDependencies { .. }
        | Command::ProjectDeploy { .. }
        | Command::DeployAbort { .. }
        | Command::DeployDelete { .. }
        | Command::DeployRollback { .. }
        | Command::ServiceStop { .. }
        | Command::ServiceReload { .. }
        | Command::ServiceWarm { .. }
        | Command::JobWarm { .. }
        | Command::JobRunNow { .. }
        | Command::JobRunCancel { .. }
        | Command::DeployQueueReorder { .. }
        | Command::DeployQueuePromote { .. }
        | Command::DeployQueuePause { .. }
        | Command::LogsExecutionDelete { .. }
        | Command::ProjectUpdate { .. }
        | Command::ProjectEnvSet { .. }
        | Command::EnvKeyRename { .. }
        | Command::EnvValueSet { .. }
        | Command::SecretSet { .. }
        | Command::SecretDelete { .. }
        | Command::SecretRename { .. }
        | Command::SecretMove { .. }
        | Command::EnvBackupRestore { .. }
        | Command::RegenerateWebhookToken { .. }
        | Command::GetWebhookUrl { .. }
        | Command::IngressReconcile { .. }
        | Command::ServiceUpdate { .. }
        | Command::StateGitSyncNow
        | Command::StateGitResume
        | Command::StateGitDismissRemoval { .. }
        | Command::JobUpdate { .. } => Level::Operator,
        Command::ProjectCreate { .. }
        | Command::ProjectDelete { .. }
        | Command::ServiceCreate(_)
        | Command::ServiceDelete { .. }
        | Command::SharedAccessGrant { .. }
        | Command::SharedAccessRevoke { .. }
        | Command::ManagedDatabaseCreate(_)
        | Command::ManagedDatabaseDelete { .. }
        | Command::MigrationStart(_)
        | Command::MigrationRollback { .. }
        | Command::MigrationDiscard { .. }
        | Command::SetDaemonSettings { .. }
        | Command::SecretReveal { .. }
        | Command::StateGitConfigure { .. }
        | Command::StateGitResolve { .. }
        | Command::InfraAuditAllow { .. }
        | Command::StopContainer { .. }
        | Command::ServiceExport { .. }
        | Command::ServiceImport(_)
        | Command::JobCreate { .. }
        | Command::JobDelete { .. }
        | Command::PruneContainers
        | Command::PruneVolumes { .. }
        | Command::PruneImages { .. }
        | Command::PruneBuildCache
        | Command::PruneNetworks
        | Command::DockerCleanupConfigSet { .. }
        | Command::DockerCleanupRunNow
        | Command::RemoveContainer { .. }
        | Command::RemoveImage { .. }
        | Command::RemoveVolume { .. }
        | Command::RemoveNetwork { .. }
        | Command::StopAllManaged
        | Command::ApplyUpdate { .. }
        | Command::UpdateRollback
        | Command::GitProviderCreate { .. }
        | Command::GitProviderDelete { .. }
        | Command::GitOAuthStart { .. }
        | Command::WizardCreate(_)
        | Command::RegistryTagDelete { .. }
        | Command::RegistryRepoDelete { .. }
        | Command::RegistryGc
        | Command::RegistryTokenCreate { .. }
        | Command::RegistryTokenRevoke { .. } => Level::Admin,
    }
}

/// `Ok` se um chamador de nível `have` pode executar `cmd`.
pub fn check(cmd: &Command, have: Level) -> Result<(), String> {
    let need = required_level(cmd);
    if need <= have {
        return Ok(());
    }
    Err(format!(
        "este comando exige o nível `{}`, e a chave de visitante tem `{}`. Use a chave mestra \
         (`rustployd access-key --master`) ou peça a quem administra para subir o `[api].visitor_level`",
        need.as_str(),
        have.as_str()
    ))
}

/// Campos de um serviço que um **operador** não pode mudar: são o que decide **o que roda**.
pub const OPERATOR_FROZEN_SPEC_FIELDS: &[&str] = &[
    "source",
    "run_command",
    "run_args",
    "volumes",
    "product",
    "shared",
];

/// Campos de um job que um **operador** não pode mudar (o que o job roda).
pub const OPERATOR_FROZEN_JOB_FIELDS: &[&str] = &["compose", "git_source", "main_service"];

/// Quais campos congelados diferem entre o job atual e o novo (JSON do `Job`).
pub fn frozen_job_changes(atual: &Value, novo: &Value) -> Vec<&'static str> {
    OPERATOR_FROZEN_JOB_FIELDS
        .iter()
        .copied()
        .filter(|campo| atual.get(campo) != novo.get(campo))
        .collect()
}

/// Quais campos congelados diferem entre o spec atual e o novo (JSON do `ServiceSpec`).
pub fn frozen_spec_changes(atual: &Value, novo: &Value) -> Vec<&'static str> {
    OPERATOR_FROZEN_SPEC_FIELDS
        .iter()
        .copied()
        .filter(|campo| atual.get(campo) != novo.get(campo))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd(j: &str) -> Command {
        serde_json::from_str(j).unwrap()
    }

    #[test]
    fn niveis_em_ordem_e_texto() {
        assert!(Level::Observer < Level::Operator && Level::Operator < Level::Admin);
        assert_eq!(Level::parse(" Operator "), Some(Level::Operator));
        assert_eq!(Level::parse("root"), None);
        assert_eq!(Level::Admin.as_str(), "admin");
    }

    #[test]
    fn exemplos_de_cada_nivel() {
        assert_eq!(required_level(&cmd(r#""ProjectList""#)), Level::Observer);
        assert_eq!(
            required_level(&cmd(r#"{"ServiceGet":{"id":"s"}}"#)),
            Level::Observer
        );
        assert_eq!(
            required_level(&cmd(r#"{"DeployStart":{"service_id":"s"}}"#)),
            Level::Operator
        );
        assert_eq!(
            required_level(&cmd(
                r#"{"EnvValueSet":{"project_id":"p","key":"A","value":{"Plain":"x"}}}"#
            )),
            Level::Operator
        );
        assert_eq!(
            required_level(&cmd(r#"{"ServiceDelete":{"id":"s"}}"#)),
            Level::Admin
        );
        assert_eq!(
            required_level(&cmd(r#"{"ProjectCreate":{"name":"x","description":null}}"#)),
            Level::Admin
        );
    }

    #[test]
    fn observador_nao_altera_nada() {
        let escrita = [
            r#"{"DeployStart":{"service_id":"s"}}"#,
            r#"{"ServiceStop":{"service_id":"s"}}"#,
            r#"{"SecretSet":{"project_id":"p","name":"N","value":"v"}}"#,
            r#"{"ProjectEnvSet":{"project_id":"p","env_vars":[],"env_comments":[]}}"#,
            r#"{"RemoveContainer":{"id":"c"}}"#,
        ];
        for c in escrita {
            assert!(
                check(&cmd(c), Level::Observer).is_err(),
                "{c} deveria exigir mais que observer"
            );
        }
        assert!(check(&cmd(r#""ProjectList""#), Level::Observer).is_ok());
        assert!(
            check(
                &cmd(r#"{"LogsGet":{"service_id":"s","tail":10}}"#),
                Level::Observer
            )
            .is_ok()
        );
    }

    #[test]
    fn operador_opera_mas_nao_cria_nem_apaga() {
        assert!(
            check(
                &cmd(r#"{"DeployStart":{"service_id":"s"}}"#),
                Level::Operator
            )
            .is_ok()
        );
        assert!(
            check(
                &cmd(r#"{"ServiceStop":{"service_id":"s"}}"#),
                Level::Operator
            )
            .is_ok()
        );
        for c in [
            r#"{"ServiceDelete":{"id":"s"}}"#,
            r#"{"ProjectDelete":{"id":"p"}}"#,
            r#"{"RemoveVolume":{"name":"v"}}"#,
            r#"{"GitProviderDelete":{"id":"g"}}"#,
        ] {
            let e = check(&cmd(c), Level::Operator).unwrap_err();
            assert!(e.contains("admin"), "{e}");
        }
        assert!(check(&cmd(r#"{"ServiceDelete":{"id":"s"}}"#), Level::Admin).is_ok());
    }

    #[test]
    fn operador_nao_muda_o_que_roda() {
        let atual = json!({
            "name":"api","replicas":1,"run_command":null,"run_args":[],"volumes":[],
            "source":{"Registry":{"image":"app:1"}},"product":null,"shared":null,
            "domains":[]
        });
        // trocar réplicas e domínios é do operador
        let mut novo = atual.clone();
        novo["replicas"] = json!(3);
        novo["domains"] = json!([{"domain":"a.com"}]);
        assert!(frozen_spec_changes(&atual, &novo).is_empty());
        // trocar a imagem, o comando ou os volumes não é
        let mut novo = atual.clone();
        novo["source"] = json!({"Registry":{"image":"app:2"}});
        novo["run_command"] = json!("sh -c 'cat /dados/age.key'");
        novo["volumes"] =
            json!([{"host_path":"/etc/rustploy","container_path":"/dados","read_only":false}]);
        assert_eq!(
            frozen_spec_changes(&atual, &novo),
            ["source", "run_command", "volumes"]
        );
    }
}
