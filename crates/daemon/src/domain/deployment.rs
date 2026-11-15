use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deployment {
    pub id: String,
    pub service_id: String,
    pub image: String,
    pub state: DeployState,
    pub states_log: Vec<StateTransition>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// O que aconteceu com um serviço num deploy em lote (`DeployWithDependencies`, `ProjectDeploy`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeployBatchStatus {
    /// Entrou na fila; `deployment_id` vem preenchido.
    Queued,
    /// Não entrou: já estava rodando (`skip_running`) ou já estava na fila/em deploy.
    Skipped,
    /// Não entrou porque uma dependência dele não pôde ser enfileirada.
    Blocked,
    /// O deploy não pôde ser criado (fonte sem configuração, erro de banco…); `message` diz a causa.
    Failed,
}

/// Um serviço do plano de um deploy em lote, na ordem em que entra na fila: as dependências
/// vêm antes de quem depende delas. `wave` é a "onda" da árvore (0 = não depende de ninguém).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeployBatchItem {
    pub service_id: String,
    pub service_name: String,
    pub wave: u32,
    pub status: DeployBatchStatus,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeployState {
    Pending,
    /// Roda a fila `ServiceSpec.pre_deploy_checks()` em ordem (quando não
    /// vazia) e só avança para `ResolvingDeps` se TODOS passarem (exit code
    /// 0) — a primeira falha interrompe a fila e o deploy. Fila vazia passa
    /// direto — no-op.
    PreDeployCheck,
    ResolvingDeps,
    PullingImage,
    CloningRepo,
    BuildingImage,
    Staging,
    HealthcheckPolling,
    SwappingIn,
    Draining,
    Promoting,
    Live,
    Stopped,
    RollingBack,
    Failed,
    Pruning,
    ComposingUp,
}

impl DeployState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Live | Self::Stopped | Self::Failed | Self::Pruning
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::PreDeployCheck => "PreDeployCheck",
            Self::ResolvingDeps => "ResolvingDeps",
            Self::PullingImage => "PullingImage",
            Self::CloningRepo => "CloningRepo",
            Self::BuildingImage => "BuildingImage",
            Self::Staging => "Staging",
            Self::HealthcheckPolling => "HealthcheckPolling",
            Self::SwappingIn => "SwappingIn",
            Self::Draining => "Draining",
            Self::Promoting => "Promoting",
            Self::Live => "Live",
            Self::Stopped => "Stopped",
            Self::RollingBack => "RollingBack",
            Self::Failed => "Failed",
            Self::Pruning => "Pruning",
            Self::ComposingUp => "ComposingUp",
        }
    }
}

#[cfg(test)]
mod pre_deploy_gate_tests {
    use super::*;
    use crate::domain::*;

    fn sample_spec(
        pre_deploy_job_id: Option<String>,
        pre_deploy_job_ids: Vec<String>,
    ) -> ServiceSpec {
        ServiceSpec {
            name: "svc".into(),
            project_id: "proj-1".into(),
            source: ServiceSource::Registry {
                image: "nginx:latest".into(),
            },
            port: 8080,
            host_port: None,
            tcp_ports: vec![],
            domain: None,
            tls_enabled: false,
            env_vars: vec![],
            env_comments: vec![],
            volumes: vec![],
            healthcheck: Healthcheck::default(),
            replicas: 1,
            resources: ResourceLimits::default(),
            run_command: None,
            run_args: vec![],
            product: None,
            domains: vec![],
            pre_deploy_job_id,
            pre_deploy_job_ids,
            shared: None,
            depends_on: vec![],
            registry_auto_deploy: false,
            autostart: true,
        }
    }

    #[test]
    fn pre_deploy_job_ids_json_round_trip() {
        let spec = sample_spec(
            None,
            vec!["job_migration".into(), "job_lint".into(), "job_test".into()],
        );
        let json = serde_json::to_string(&spec).unwrap();
        let back: ServiceSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.pre_deploy_job_ids,
            vec![
                "job_migration".to_string(),
                "job_lint".to_string(),
                "job_test".to_string()
            ]
        );
    }

    #[test]
    fn pre_deploy_job_ids_back_compat_when_absent() {
        // Specs salvos antes da feature não têm o campo; serde default
        // precisa preencher como vec vazio (mesmo padrão de domains/etc.).
        let spec = sample_spec(None, vec![]);
        let mut value = serde_json::to_value(&spec).unwrap();
        value.as_object_mut().unwrap().remove("pre_deploy_job_ids");
        let back: ServiceSpec = serde_json::from_value(value).unwrap();
        assert_eq!(back.pre_deploy_job_ids, Vec::<String>::new());
    }

    #[test]
    fn pre_deploy_checks_falls_back_to_legacy_single_job() {
        let spec = sample_spec(Some("job_legacy".into()), vec![]);
        assert_eq!(spec.pre_deploy_checks(), vec!["job_legacy".to_string()]);
    }

    #[test]
    fn pre_deploy_checks_prefers_queue_over_legacy() {
        let spec = sample_spec(
            Some("job_legacy".into()),
            vec!["job_a".into(), "job_b".into()],
        );
        assert_eq!(
            spec.pre_deploy_checks(),
            vec!["job_a".to_string(), "job_b".to_string()]
        );
    }

    #[test]
    fn materialize_pre_deploy_checks_moves_legacy_and_clears_it() {
        let mut spec = sample_spec(Some("job_legacy".into()), vec![]);
        spec.materialize_pre_deploy_checks();
        assert_eq!(spec.pre_deploy_job_ids, vec!["job_legacy".to_string()]);
        assert_eq!(spec.pre_deploy_job_id, None);
    }

    #[test]
    fn deploy_state_pre_deploy_check_label_and_serde() {
        assert_eq!(DeployState::PreDeployCheck.label(), "PreDeployCheck");
        let json = serde_json::to_string(&DeployState::PreDeployCheck).unwrap();
        assert_eq!(json, "\"PreDeployCheck\"");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateTransition {
    pub from: DeployState,
    pub to: DeployState,
    pub at: DateTime<Utc>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentSummary {
    pub deployment: Deployment,
    pub service_name: String,
    pub project_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub uptime_secs: u64,
    pub services_running: usize,
    pub services_total: usize,
}

/// O que um item da fila global é: um deploy, a execução de um job avulso (Agendamentos) ou o
/// aquecimento de um job. Os três disputam a mesma vaga única e aparecem nas mesmas listas do
/// Deploy Engine (`queued`, `active`, `recent`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueItemKind {
    #[default]
    Deploy,
    /// Uma execução de job (`job_run`). `deployment_id` carrega o id da execução (`jrun_…`).
    Job,
    /// O aquecimento de um job (`jwarm_…`). Só existe enquanto espera/roda: não vai ao histórico.
    Warm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveDeployInfo {
    pub deployment_id: String,
    pub service_id: String,
    pub service_name: String,
    pub project_name: String,
    pub state: DeployState,
    pub started_at: DateTime<Utc>,
    pub elapsed_secs: u64,
    pub current_state_secs: u64,
    /// Histórico de transições do deploy (o `states_log`), em ordem. Alimenta o
    /// stepper detalhado dos clientes; daemons antigos não o enviam.
    #[serde(default)]
    pub states: Vec<StateTransition>,
    /// Deploy, job ou aquecimento. Daemon antigo não manda: `Deploy`. Para `Job` e `Warm`,
    /// `deployment_id` é o id do item na fila (serve a promover, reordenar e cancelar),
    /// `service_name` é o nome do job e `service_id` o do serviço gatilho (vazio se autônomo).
    #[serde(default)]
    pub kind: QueueItemKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEngineSummary {
    pub version: String,
    pub uptime_secs: u64,
    pub active: Vec<ActiveDeployInfo>,
    pub recent: Vec<ActiveDeployInfo>,
    pub total_24h: u64,
    pub successful_24h: u64,
    pub failed_24h: u64,
    /// Deploys esperando na fila global (em ordem de execução; o primeiro é o
    /// próximo a rodar).
    pub queued: Vec<ActiveDeployInfo>,
    /// Fila pausada — o worker não puxa o próximo até retomar.
    pub paused: bool,
}

#[cfg(test)]
mod queue_item_kind_tests {
    use super::*;

    #[test]
    fn daemon_antigo_sem_kind_e_deploy() {
        let json = r#"{"deployment_id":"dep_1","service_id":"s","service_name":"api","project_name":"p",
            "state":"Pending","started_at":"2026-11-10T00:00:00Z","elapsed_secs":0,"current_state_secs":0}"#;
        let info: ActiveDeployInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.kind, QueueItemKind::Deploy);
    }

    #[test]
    fn kind_de_job_vai_e_volta() {
        let json = serde_json::to_string(&QueueItemKind::Job).unwrap();
        assert_eq!(json, "\"Job\"");
        assert_eq!(
            serde_json::from_str::<QueueItemKind>("\"Warm\"").unwrap(),
            QueueItemKind::Warm
        );
    }
}
