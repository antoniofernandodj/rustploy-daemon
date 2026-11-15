//! `Command::ServiceReload`: reinicia o container live de um serviço (stop
//! gracioso + start) sem redeployar.

use crate::domain::{DeployState, ServiceStatus};
use crate::protocol::{Event, Response as RpResponse};
use crate::{bootstrap::state::AppState, engine::containers};
use chrono::Utc;

pub async fn handle(state: AppState, service_id: String) -> RpResponse {
    let svc = match crate::store::services::get(&state.db, &service_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return RpResponse::err("NotFound", "serviço não encontrado"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };

    let container_id = match &svc.live_container_id {
        Some(id) => id.clone(),
        None => return RpResponse::err("NotRunning", "serviço não possui container ativo"),
    };

    if let Err(e) = containers::stop_graceful(&state.docker.inner, &container_id, 10).await {
        return RpResponse::err("RuntimeError", format!("falha ao parar: {e}"));
    }

    if let Err(e) = containers::start(&state.docker.inner, &container_id).await {
        return RpResponse::err("RuntimeError", format!("falha ao iniciar: {e}"));
    }

    if let Err(e) = crate::store::services::update_status(
        &state.db,
        &service_id,
        &ServiceStatus::Running,
        Some(&container_id),
    )
    .await
    {
        return RpResponse::err("DatabaseError", e.to_string());
    }

    // Se o deployment estava Stopped, volta para Live.
    if let Ok(history) =
        crate::store::deployments::list_for_service(&state.db, &service_id, 1).await
        && let Some(dep) = history
            .into_iter()
            .find(|d| d.state == DeployState::Stopped)
    {
        let _ = crate::store::deployments::transition(
            &state.db,
            &dep.id,
            &DeployState::Stopped,
            DeployState::Live,
            None,
        )
        .await;
        state.bus.publish(Event::DeployStateChanged {
            deployment_id: dep.id,
            service_id: service_id.clone(),
            state: DeployState::Live,
            timestamp: Utc::now(),
            message: None,
        });
    }

    state.bus.publish(Event::ServiceStatusChanged {
        service_id,
        status: ServiceStatus::Running,
    });

    RpResponse::Ok
}
