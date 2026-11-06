//! `Command::LogsExecutions` e `Command::LogsExecutionDelete`: as execuções (um container = uma
//! execução) com log guardado, e a exclusão delas. Plano: `docs/plano-log-de-runtime-persistido.md`.

use crate::bootstrap::state::AppState;
use crate::platform::log_store::{self, ExecMeta};
use shared::protocol::LogExecution;
use shared::{Response as RpResponse, ServiceSource};

/// Serviço existente e com log persistido ligado, ou a resposta de erro pronta.
async fn service_and_store(
    state: &AppState,
    service_id: &str,
) -> Result<(shared::Service, std::sync::Arc<log_store::LogStore>), RpResponse> {
    let svc = match crate::store::services::get(&state.db, service_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(RpResponse::err("NotFound", "serviço não encontrado")),
        Err(e) => return Err(RpResponse::err("DatabaseError", e.to_string())),
    };
    let Some(store) = log_store::global() else {
        return Err(RpResponse::err(
            "LogsDisabled",
            "o log persistido está desligado ([logs] enabled = false no config.toml)",
        ));
    };
    Ok((svc, store))
}

/// A execução em curso é a do container `live` do serviço.
fn is_current(live_container_id: Option<&str>, meta: &ExecMeta) -> bool {
    live_container_id.is_some_and(|id| log_store::short_id(id) == meta.container)
}

pub async fn list(state: AppState, service_id: String) -> RpResponse {
    let (svc, store) = match service_and_store(&state, &service_id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Compose segue só ao vivo: não há log guardado.
    if matches!(svc.spec.source, ServiceSource::Compose(_)) {
        return RpResponse::LogsExecutions(Vec::new());
    }
    let sid = service_id.clone();
    let metas = match tokio::task::spawn_blocking(move || store.executions(&sid)).await {
        Ok(m) => m,
        Err(e) => return RpResponse::err("InternalError", e.to_string()),
    };
    let mut out = Vec::with_capacity(metas.len());
    for m in metas {
        // Sem registro de início/fim (pasta vazia): nada a mostrar.
        let Some(started_at) = m.first_ts else {
            continue;
        };
        let outcome = match &m.deployment_id {
            Some(id) => crate::store::deployments::get(&state.db, id)
                .await
                .ok()
                .flatten()
                .map(|d| format!("{:?}", d.state)),
            None => None,
        };
        out.push(LogExecution {
            current: is_current(svc.live_container_id.as_deref(), &m),
            container_ids: vec![m.container],
            deployment_id: m.deployment_id,
            image: m.image,
            started_at,
            ended_at: m.last_ts,
            lines: m.lines,
            bytes: m.bytes,
            partial: m.partial,
            outcome,
        });
    }
    RpResponse::LogsExecutions(out)
}

pub async fn delete(state: AppState, service_id: String, container_ids: Vec<String>) -> RpResponse {
    let (svc, store) = match service_and_store(&state, &service_id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // A execução atual não sai: o escritor ainda grava nela.
    if let Some(live) = svc.live_container_id.as_deref() {
        let live = log_store::short_id(live);
        if container_ids.iter().any(|c| log_store::short_id(c) == live) {
            return RpResponse::err(
                "ExecutionRunning",
                "a execução atual não pode ser excluída (ela ainda está gravando)",
            );
        }
    }
    let sid = service_id.clone();
    let ids = container_ids.clone();
    match tokio::task::spawn_blocking(move || store.delete_executions(&sid, &ids)).await {
        Ok((deleted, freed_bytes)) => {
            tracing::info!(service = %service_id, deleted, freed_bytes, "logs: execuções excluídas");
            RpResponse::LogsExecutionsDeleted {
                deleted,
                freed_bytes,
            }
        }
        Err(e) => RpResponse::err("InternalError", e.to_string()),
    }
}
