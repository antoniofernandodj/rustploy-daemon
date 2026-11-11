//! Um diretório por assunto de produto; a camada `api` só adapta pedidos para estas funcionalidades.

pub mod backup;
pub mod data_backup;
pub mod databases;
pub mod deploy;
pub mod git_providers;
pub mod iac;
pub mod ingress;
pub mod jobs;
pub mod maintenance;
pub mod observability;
pub mod registry;
pub mod service_data;
pub mod services;
pub mod state_git;
pub mod templates;
pub mod update;
pub mod wizard;
