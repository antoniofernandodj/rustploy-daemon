//! Motor de deploy: fila global, executor, recuperação no boot, clone git e
//! resolução de env vars.

mod context;
pub mod env_resolve;
pub mod executor;
pub mod git;
pub mod queue;
pub mod recovery;
pub mod shared_net;
pub mod start;
pub mod stop;

pub use context::{ActiveDeploys, DeployContext};
