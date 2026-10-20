//! Limpeza automática (agendada) de recursos Docker não usados — ver
//! `docs/plano-limpeza-automatica-docker.md`. Config é um singleton em
//! `daemon_settings` (não uma tabela própria); a execução reaproveita as
//! mesmas funções `*_core` que os botões manuais da aba Docker chamam
//! (`prune`).

mod context;
pub mod prune;
pub mod run;
pub mod scheduler;
pub mod service_warm;

pub use context::MaintenanceContext;
