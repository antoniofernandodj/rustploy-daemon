//! Handlers agrupados por assunto: um arquivo por `Command` dentro de cada pasta, e o `dispatch` da pasta
//! escolhe o handler. O primeiro nível do `match` fica em `api/routes.rs`.

pub mod databases;
pub mod deploys;
pub mod docker;
pub mod git;
pub mod iac;
pub mod jobs;
pub mod observability;
pub mod projects;
pub mod registry;
pub mod secrets;
pub mod services;
pub mod settings;
