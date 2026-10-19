//! Destino das linhas de log de build/job: o motor de containers escreve aqui sem saber que existe banco.

use chrono::{DateTime, Utc};
use shared::protocol::LogStream;
use std::future::Future;

pub trait LogSink: Send + Sync + 'static {
    fn append_build_line(
        &self,
        deployment_id: &str,
        line: &str,
        timestamp: DateTime<Utc>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    fn append_job_line(
        &self,
        job_run_id: &str,
        stream: &LogStream,
        line: &str,
        timestamp: DateTime<Utc>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

impl<L: LogSink> LogSink for std::sync::Arc<L> {
    fn append_build_line(
        &self,
        deployment_id: &str,
        line: &str,
        timestamp: DateTime<Utc>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        (**self).append_build_line(deployment_id, line, timestamp)
    }

    fn append_job_line(
        &self,
        job_run_id: &str,
        stream: &LogStream,
        line: &str,
        timestamp: DateTime<Utc>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        (**self).append_job_line(job_run_id, stream, line, timestamp)
    }
}
