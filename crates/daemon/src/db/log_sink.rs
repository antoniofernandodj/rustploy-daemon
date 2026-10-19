//! Grava as linhas de log de build/job nas tabelas `build_log` e `job_log`.

use super::{Db, build_logs, job_log};
use crate::platform::log_sink::LogSink;
use chrono::{DateTime, Utc};
use shared::protocol::LogStream;

impl LogSink for Db {
    async fn append_build_line(
        &self,
        deployment_id: &str,
        line: &str,
        timestamp: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        build_logs::append(self, deployment_id, line, timestamp).await
    }

    async fn append_job_line(
        &self,
        job_run_id: &str,
        stream: &LogStream,
        line: &str,
        timestamp: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        job_log::append(self, job_run_id, stream, line, timestamp).await
    }
}
