//! Protocolo da API: `Command` (o que o cliente pede), `Response` e `Event` (o que o SSE entrega).

mod command;
mod event;
mod logs;
mod response;

pub use command::{Command, SecretMoveOp, StateGitResolveChoice};
pub use event::Event;
pub use logs::{BuildLogLine, LogEntry, LogExecution, LogHistoryEntry, LogStream};
pub use response::Response;
