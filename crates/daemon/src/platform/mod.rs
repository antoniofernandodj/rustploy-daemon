//! Infraestrutura transversal (barramento de eventos, segredos, health checks), sem regra de negócio.

pub mod config;
pub mod connection;
pub mod errors;
pub mod event_bus;
pub mod health;
pub mod log_sink;
pub mod secrets;
