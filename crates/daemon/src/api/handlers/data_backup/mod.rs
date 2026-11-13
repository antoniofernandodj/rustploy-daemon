//! Handlers do backup de dados para S3: o `dispatch` recebe os `Command` da seção e chama o handler certo.
//! Plano: `docs/plano-backup-de-dados-para-s3.md`.

mod backup_destination_clear;
mod backup_destination_get;
mod backup_destination_set;
mod backup_destination_test;
mod recovery_key_generate;
mod recovery_key_status;
mod server_recover;
mod service_backup_delete;
mod service_backup_list;
mod service_backup_now;
mod service_backup_policy_set;
mod service_backup_restore;

use crate::bootstrap::state::AppState;
use shared::{Command, Response as RpResponse};

pub async fn dispatch(state: AppState, cmd: Command) -> RpResponse {
    match cmd {
        Command::BackupDestinationGet => backup_destination_get::handle(state).await,
        Command::BackupDestinationSet { destination } => {
            backup_destination_set::handle(state, destination).await
        }
        Command::BackupDestinationClear => backup_destination_clear::handle(state).await,
        Command::BackupDestinationTest { destination } => {
            backup_destination_test::handle(state, destination).await
        }
        Command::ServerRecoverStart {
            remote,
            token,
            branch,
            recovery_key,
        } => server_recover::start(state, remote, token, branch, recovery_key).await,
        Command::ServerRecoverStatus => server_recover::status().await,
        Command::ServerRecoverRun => server_recover::run(state).await,
        Command::ServerRecoverCancel => server_recover::cancel(state).await,
        Command::RecoveryKeyStatus => recovery_key_status::handle(&state),
        Command::RecoveryKeyGenerate { replace } => recovery_key_generate::handle(&state, replace),
        Command::ServiceBackupList { service_id } => {
            service_backup_list::handle(state, service_id).await
        }
        Command::ServiceBackupNow {
            service_id,
            stop,
            keep,
        } => service_backup_now::handle(state, service_id, stop, keep).await,
        Command::ServiceBackupPolicySet { service_id, policy } => {
            service_backup_policy_set::handle(state, service_id, policy).await
        }
        Command::ServiceBackupRestore {
            service_id,
            key,
            confirm,
        } => service_backup_restore::handle(state, service_id, key, confirm).await,
        Command::ServiceBackupDelete { service_id, key } => {
            service_backup_delete::handle(state, service_id, key).await
        }
        _ => RpResponse::err("NotImplemented", "command not yet implemented"),
    }
}
