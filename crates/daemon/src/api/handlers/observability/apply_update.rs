//! `Command::ApplyUpdate`: atualiza o daemon para a última release estável.
//!
//! O handler só **valida e dispara**; o trabalho (snapshot do banco, download, pedido ao helper
//! root) roda em segundo plano, porque o `dpkg` reinicia o daemon no meio e nenhuma resposta
//! HTTP sobreviveria a isso. A UI acompanha por `Command::UpdateStatus`.

use crate::{bootstrap::state::AppState, features::update::apply::ApplyPlan};
use shared::Response as RpResponse;

pub async fn handle(state: AppState, allow_active_deploys: bool) -> RpResponse {
    if !state.update_applier.is_helper_available() {
        return RpResponse::err(
            "UpdateHelperUnavailable",
            "o helper de atualização não está disponível nesta máquina (sem systemd, ou instalação que \
             não veio do pacote .deb); atualize pelo comando de instalação",
        );
    }
    let info = match state.update_checker.check(false).await {
        Ok(info) => info,
        Err(error) => return RpResponse::err("UpdateCheckFailed", error.to_string()),
    };
    if !info.is_update_available {
        return RpResponse::err(
            "NoUpdateAvailable",
            "o daemon já está na versão mais recente",
        );
    }
    let Some(release) = state.update_checker.cached_release().await else {
        return RpResponse::err(
            "UpdateCheckFailed",
            "não há release consultada; verifique de novo",
        );
    };

    if !allow_active_deploys {
        let (running, queued, _paused) = state.deploy_queue.snapshot();
        if running.is_some() || !queued.is_empty() {
            return RpResponse::err(
                "DeploysInProgress",
                "há deploy em andamento ou na fila; o reinício do daemon o interromperia. Espere terminar \
                 ou confirme que quer atualizar mesmo assim",
            );
        }
    }

    if let Err(message) = state.update_applier.try_begin(&info.latest_version) {
        return RpResponse::err("UpdateInProgress", message);
    }

    let plan = ApplyPlan {
        release,
        repo: state.update_checker.repo().to_string(),
        db: state.db.clone(),
        snapshots_dir: state.db_path.join("update_snapshots"),
        current_version: info.current_version,
    };
    let applier = state.update_applier.clone();
    tokio::spawn(async move { applier.run(plan).await });
    RpResponse::Ok
}
