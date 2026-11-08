//! Motor de deploy: fila global, executor, recuperação no boot, clone git e
//! resolução de env vars.

pub mod autostart;
mod context;
pub mod deps;
pub mod env_resolve;
pub mod executor;
pub mod git;
pub mod queue;
pub mod recovery;
pub mod shared_net;
pub mod start;
pub mod stop;
pub mod tree;

pub use context::{ActiveDeploys, DeployContext};
