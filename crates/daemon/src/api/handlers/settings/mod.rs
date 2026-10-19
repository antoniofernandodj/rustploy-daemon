//! Handlers de settings: o `dispatch` recebe os `Command` da seção e chama o handler certo.

pub mod env_backup;
pub mod get_daemon_settings;
pub mod set_daemon_settings;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::EnvBackupList => env_backup::list(state).await,
        Command::EnvBackupRestore { snapshot } => env_backup::restore(state, snapshot).await,
        Command::GetDaemonSettings => get_daemon_settings::handle(state).await,
        Command::SetDaemonSettings {
            acme_email,
            registry_domain,
        } => set_daemon_settings::handle(state, acme_email, registry_domain).await,
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
