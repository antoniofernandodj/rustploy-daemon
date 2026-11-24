//! `Command::ServiceDelete`: para e REMOVE os containers do serviço (ou a stack
//! Compose), apaga o serviço, tira suas rotas do ingress e fecha a porta
//! externa no firewall. Imagens ficam. Os volumes nomeados (os dados) ficam por padrão e são
//! anotados como órfãos (`store::orphan_volumes`); com `delete_volumes`, saem junto. Os emprestados
//! (externos do Compose) só saem com `delete_external_volumes`.

use crate::domain::ServiceSource;
use crate::engine::containers;
use crate::features::services::ServicesContext;
use crate::protocol::Response as RpResponse;

/// Containers do serviço: para (réplicas, ou `compose down`, que já os remove) e
/// remove o que sobrar, inclusive staging e parados. Idempotente: sem container
/// algum, é um no-op. Falha de motor = erro (o serviço não é apagado; dá para repetir).
async fn remove_containers(state: &ServicesContext, id: &str) -> Result<(), RpResponse> {
    let Ok(Some(svc)) = crate::store::services::get(&state.db, id).await else {
        return Ok(());
    };
    // Reaproveita o ServiceStop: Application para as réplicas; Compose roda `compose down`
    // (que remove os containers e a rede, mas NÃO os volumes).
    if let err @ RpResponse::Err { .. } =
        crate::features::deploy::stop::handle(state.deploy.clone(), id.to_string()).await
    {
        return Err(err);
    }
    if !matches!(svc.spec.source, ServiceSource::Compose(_)) {
        let ids = containers::find_all_by_service_id(&state.docker.inner, id)
            .await
            .map_err(|e| RpResponse::err("RuntimeError", e.to_string()))?;
        for cid in ids {
            containers::remove(&state.docker.inner, &cid)
                .await
                .map_err(|e| RpResponse::err("RuntimeError", e.to_string()))?;
        }
    }
    Ok(())
}

/// O que fazer com os volumes ao apagar: os **próprios** do serviço e os **emprestados** (externos
/// do Compose). `false` = mantém (vira órfão, com a origem anotada).
#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeChoice {
    pub own: bool,
    pub borrowed: bool,
}

pub async fn handle(state: ServicesContext, id: String, choice: VolumeChoice) -> RpResponse {
    // Guarda: não apaga um serviço do qual outros dependem (o autostart os deixaria bloqueados).
    if let Ok(Some(svc)) = crate::store::services::get(&state.db, &id).await
        && let Ok(all) =
            crate::features::services::project_deps(&state.db, &svc.spec.project_id).await
    {
        let dependents = crate::features::deploy::deps::direct_dependents(&all, &id);
        if !dependents.is_empty() {
            let names: Vec<String> = dependents.into_iter().map(|(_, n)| n).collect();
            return RpResponse::err(
                "DependencyInUse",
                format!(
                    "outros serviços dependem deste: {} — remova a dependência neles antes de apagar",
                    names.join(", ")
                ),
            );
        }
    }
    // Os volumes do serviço, antes de ele sumir do estado (depois não há mais como achá-los).
    let (volumes, borrowed, service_name, project_name) = match crate::store::services::get(
        &state.db, &id,
    )
    .await
    {
        Ok(Some(svc)) => {
            let project_name = crate::store::projects::get(&state.db, &svc.spec.project_id)
                .await
                .ok()
                .flatten()
                .map(|project| project.name)
                .unwrap_or_default();
            let volumes = crate::features::service_data::parts::named_volumes(&svc)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(service_id = %id, error = %e, "service_delete: não listei os volumes");
                        Vec::new()
                    });
            let borrowed = crate::features::service_data::parts::borrowed_volumes(&svc);
            (volumes, borrowed, svc.spec.name.clone(), project_name)
        }
        _ => (Vec::new(), Vec::new(), String::new(), String::new()),
    };
    // Primeiro os containers: se o motor falhar, nada mudou (rotas, registro) e dá para tentar de novo.
    if let Err(e) = remove_containers(&state, &id).await {
        return e;
    }
    let mut host_ports = Vec::new();
    if let Ok(Some(svc)) = crate::store::services::get(&state.db, &id).await {
        state.ingress.remove_domains(&svc.spec);
        state.ingress.forget_service(&id);
        host_ports = crate::features::ingress::external_ports::host_ports(&svc.spec);
        for port in &host_ports {
            state.ingress.remove_port_route(*port);
        }
    }
    match crate::store::services::delete(&state.db, &id).await {
        Ok(true) => {
            // Os dados: apagados a pedido, ou anotados como órfãos (de quem eram).
            // Emprestados (externos do Compose) só saem com a escolha própria deles.
            let decisions = volumes
                .iter()
                .map(|volume| (volume, choice.own))
                .chain(borrowed.iter().map(|volume| (volume, choice.borrowed)));
            for (volume, delete_it) in decisions {
                if delete_it {
                    match state
                        .docker
                        .inner
                        .remove_volume(
                            volume,
                            Some(bollard::volume::RemoveVolumeOptions { force: false }),
                        )
                        .await
                    {
                        Ok(()) => {
                            tracing::info!(volume = %volume, "service_delete: volume apagado")
                        }
                        Err(e) => {
                            tracing::warn!(volume = %volume, error = %e, "service_delete: volume não apagado; fica como órfão");
                            let _ = crate::store::orphan_volumes::record(
                                &state.db,
                                volume,
                                &service_name,
                                &project_name,
                            )
                            .await;
                        }
                    }
                } else if let Err(e) = crate::store::orphan_volumes::record(
                    &state.db,
                    volume,
                    &service_name,
                    &project_name,
                )
                .await
                {
                    tracing::warn!(volume = %volume, error = %e, "service_delete: não anotei o volume órfão");
                }
            }
            // As releases (o "voltar para a vN") não têm mais a que voltar.
            if let Err(e) = crate::store::releases::delete_for_service(&state.db, &id).await {
                tracing::warn!(service_id = %id, error = %e, "service_delete: falha ao remover as releases");
            }
            // O histórico de log do serviço vai junto.
            if let Some(store) = crate::platform::log_store::global() {
                store.delete_service(&id);
            }
            // Sem regra órfã: com o serviço fora do DB, fecha a porta no
            // firewall (a menos que outro serviço compartilhe a mesma porta).
            for port in host_ports {
                if !crate::features::ingress::external_ports::port_in_use_by_other(
                    &state.db,
                    port,
                    Some(&id),
                )
                .await
                {
                    crate::features::ingress::firewall::ensure_denied_bg(port);
                }
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
