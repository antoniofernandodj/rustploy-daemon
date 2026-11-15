//! Linhas de log: stream de origem e entradas de container/build.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub stream: LogStream,
    pub line: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Uma linha do log de runtime **persistido** (`LogsHistory`): além do texto, de qual container veio,
/// porque o histórico atravessa trocas de container (deploys).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogHistoryEntry {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Id curto (12 caracteres) do container que gerou a linha.
    pub container_id: String,
    pub stream: LogStream,
    pub line: String,
}

/// Uma execução (um container) com log guardado: resposta de `LogsExecutions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogExecution {
    /// Ids curtos dos containers desta execução (hoje sempre um).
    pub container_ids: Vec<String>,
    /// Deploy que criou o container; `None` nas execuções anteriores ao registro (migradas).
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub lines: u64,
    pub bytes: u64,
    /// É a execução em curso (o container `live` agora).
    pub current: bool,
    /// Segmentos do começo foram apagados pela rotação de tamanho.
    pub partial: bool,
    /// Desfecho do deploy: `Live`, `Stopped` (substituída), `Failed`…; `None` sem deploy associado.
    #[serde(default)]
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildLogLine {
    pub stream: LogStream,
    pub line: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}
