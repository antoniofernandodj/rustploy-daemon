//! `Command::ServiceDelete`: para e REMOVE os containers do serviço (ou a stack
//! Compose), apaga o serviço, tira suas rotas do ingress e fecha a porta
//! externa no firewall. Volumes nomeados e imagens ficam (dados do usuário).

use crate::{bootstrap::state::AppState, engine::containers};
use shared::{Response as RpResponse, ServiceSource};

/// Containers do serviço: para (réplicas, ou `compose down`, que já os remove) e
/// remove o que sobrar, inclusive staging e parados. Idempotente: sem container
/// algum, é um no-op. Falha de motor = erro (o serviço não é apagado; dá para repetir).
async fn remove_containers(state: &AppState, id: &str) -> Result<(), RpResponse> {
    let Ok(Some(svc)) = crate::store::services::get(&state.db, id).await else {
        return Ok(());
    };
    // Reaproveita o ServiceStop: Application para as réplicas; Compose roda `compose down`
    // (que remove os containers e a rede, mas NÃO os volumes).
    if let err @ RpResponse::Err { .. } =
        crate::features::deploy::stop::handle(state.clone(), id.to_string()).await
    {
        return Err(err);
    }
    if !matches!(svc.spec.source, ServiceSource::Compose(_)) {
        let ids = containers::find_all_by_service_id(&state.docker.inner, id)
            .await
            .map_err(|e| RpResponse::err("DockerError", e.to_string()))?;
        for cid in ids {
            containers::remove(&state.docker.inner, &cid)
                .await
                .map_err(|e| RpResponse::err("DockerError", e.to_string()))?;
        }
    }
    Ok(())
}

pub async fn handle(state: AppState, id: String) -> RpResponse {
    // Primeiro os containers: se o motor falhar, nada mudou (rotas, registro) e dá para tentar de novo.
    if let Err(e) = remove_containers(&state, &id).await {
        return e;
    }
    let mut host_port = None;
    if let Ok(Some(svc)) = crate::store::services::get(&state.db, &id).await {
        state.ingress.remove_domains(&svc.spec);
        if let Some(port) = svc.spec.host_port {
            state.ingress.remove_port_route(port);
            host_port = Some(port);
        }
    }
    match crate::store::services::delete(&state.db, &id).await {
        Ok(true) => {
            // Sem regra órfã: com o serviço fora do DB, fecha a porta no
            // firewall (a menos que outro serviço compartilhe a mesma porta).
            if let Some(port) = host_port
                && !crate::features::ingress::external_ports::port_in_use_by_other(
                    &state.db,
                    port,
                    Some(&id),
                )
                .await
            {
                crate::features::ingress::firewall::ensure_denied_bg(port);
            }
            // Jobs gatilhados por este serviço vão junto (senão ficariam
            // órfãos, falhando em loop a cada recorrência) — a GUI avisa no
            // diálogo de confirmação antes de chegar aqui.
            match crate::store::job::delete_by_trigger_service(&state.db, &id).await {
                Ok(n) if n > 0 => {
                    tracing::info!(service_id = %id, jobs = n, "service_delete: jobs gatilhados removidos em cascata");
                }
                Err(e) => {
                    tracing::warn!(service_id = %id, error = %e, "service_delete: falha ao remover jobs gatilhados (ficarão órfãos)");
                }
                _ => {}
            }
            RpResponse::Ok
        }
        Ok(false) => RpResponse::err("NotFound", "service not found"),
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}
