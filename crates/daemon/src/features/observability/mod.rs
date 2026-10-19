//! Observabilidade: watchdog dos serviços, métricas do host e transmissão de logs de container.

pub mod container_logs;
mod context;
pub mod metrics;
pub mod watchdog;

pub use context::WatchdogContext;
