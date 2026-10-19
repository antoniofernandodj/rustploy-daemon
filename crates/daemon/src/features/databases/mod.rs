//! Bancos de dados gerenciados: assistente de migração, acesso compartilhado e troca de variável de conexão.

mod context;
pub mod env_switch;
pub mod managed_view;
pub mod migration;
pub mod shared_db;

pub use context::MigrationContext;
