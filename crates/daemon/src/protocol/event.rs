//! `Event`: o que o SSE entrega ao cliente.

use crate::domain::*;
use serde::{Deserialize, Serialize};

use super::LogStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    DeployStateChanged {
        deployment_id: String,
        service_id: String,
        state: DeployState,
        timestamp: chrono::DateTime<chrono::Utc>,
        message: Option<String>,
    },
    DeployProgress {
        deployment_id: String,
        service_id: String,
        phase: String,
        percent: u8,
        description: String,
    },
    /// Output from `docker build` — belongs to a specific deployment.
    BuildLog {
        deployment_id: String,
        service_id: String,
        line: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// stdout/stderr of the running container — belongs to the service.
    LogLine {
        service_id: String,
        container_id: String,
        stream: LogStream,
        line: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    ContainerMetrics(ContainerMetricsPoint),
    SystemMetrics(SystemMetricsPoint),
    ServiceStatusChanged {
        service_id: String,
        status: ServiceStatus,
    },
    DaemonReady {
        version: String,
    },
    Error {
        code: String,
        message: String,
    },
    /// Uma linha de stdout/stderr do `docker compose` de um `JobRun` (mesma
    /// forma de `Event::BuildLog`).
    JobLogLine {
        job_run_id: String,
        job_id: String,
        line: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        stream: LogStream,
    },
    /// Início/fim de uma execução de job.
    JobRunStateChanged {
        job_id: String,
        job_run_id: String,
        running: bool,
        success: Option<bool>,
    },
    /// A fila global de deploys mudou (enfileirou, começou, terminou, reordenou,
    /// cancelou ou pausou/retomou). Sinal leve: o cliente refaz o
    /// `DeployEngineStatus` ao recebê-lo. Anexado no fim do enum de propósito.
    DeployQueueChanged,
    /// Uma limpeza automática (ou "Executar agora") terminou — disparada pelo
    /// `maintenance::scheduler_loop` ou por `Command::DockerCleanupRunNow`.
    /// Não é por-serviço, então cai no braço "sempre true" de `matches`.
    DockerCleanupCompleted {
        at: chrono::DateTime<chrono::Utc>,
        results: Vec<DockerCleanupResourceResult>,
    },
    /// Um aquecimento de job começou (`warming: true`) ou terminou
    /// (`warming: false`, com `success` e, na falha, `error`).
    JobWarmChanged {
        job_id: String,
        warming: bool,
        success: Option<bool>,
        error: Option<String>,
    },
    /// Um aquecimento de serviço começou (`warming: true`) ou terminou
    /// (`warming: false`, com `success` e, na falha, `error`). Vale pra todos os
    /// clientes (não filtra por serviço): a tela de lista precisa dele.
    ServiceWarmChanged {
        service_id: String,
        warming: bool,
        success: Option<bool>,
        error: Option<String>,
    },
    /// Um `PUT /v2/<repo>/manifests/<tag>` do registry embutido foi aceito: a imagem
    /// `<repo>:<tag>` mudou. Vale para todos os clientes (a aba Registry atualiza ao vivo) e é o
    /// gatilho do push-to-deploy dos serviços com `registry_auto_deploy`.
    RegistryPush {
        repo: String,
        tag: String,
        digest: String,
    },
}

impl Event {
    pub fn matches(&self, service_id: &str) -> bool {
        match self {
            Event::DeployStateChanged {
                service_id: sid, ..
            } => sid == service_id,
            Event::DeployProgress {
                service_id: sid, ..
            } => sid == service_id,
            Event::BuildLog {
                service_id: sid, ..
            } => sid == service_id,
            Event::LogLine {
                service_id: sid, ..
            } => sid == service_id,
            Event::ContainerMetrics(m) => m.service_id == service_id,
            Event::ServiceStatusChanged {
                service_id: sid, ..
            } => sid == service_id,
            Event::DaemonReady { .. }
            | Event::Error { .. }
            | Event::SystemMetrics(_)
            | Event::JobLogLine { .. }
            | Event::JobRunStateChanged { .. }
            | Event::DeployQueueChanged
            | Event::DockerCleanupCompleted { .. }
            | Event::JobWarmChanged { .. }
            | Event::ServiceWarmChanged { .. }
            | Event::RegistryPush { .. } => true,
        }
    }
}
