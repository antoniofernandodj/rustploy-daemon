//! Ciclo de vida de um serviço: criar, atualizar (com a sincronização do firewall) e remover.

mod context;
pub mod create;
pub mod delete;
pub mod update;

pub use context::ServicesContext;
