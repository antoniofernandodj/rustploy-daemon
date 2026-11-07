//! Corrige deployments marcados Live que já não estão no ar (`fix_stale_live`),
//! sem tocar em serviço com deploy em andamento.

/// Reconciles deployments marked as "Live" in the DB against the actual Docker
/// container state, correcting stale entries.
///
/// Rules:
///   1. For each service, only the most recent deployment can be Live.
///      All older Live deployments for the same service → Pruning.
///   2. For the most recent Live deployment per service, verify its container
///      is actually running. If not → Stopped.
use crate::store::services::DesiredState;
use crate::{bootstrap::state::AppState, engine};
use chrono::Utc;
use shared::{DeployState, Deployment, Event, ServiceStatus};
use std::collections::HashSet;
use std::time::Duration;
use tracing::{info, warn};

/// Mensagem da transição `Live → Stopped` feita por este reconcile. É também a marca que
/// [`is_reconcile_stop`] procura para saber que o `Stopped` não foi decisão de ninguém.
const STOPPED_BY_RECONCILE: &str = "container not running at read time";

/// Janela, depois do boot do daemon, em que um container parado de um serviço que a pessoa quer no ar
/// ainda **não subiu** — o autostart o religa em seguida (e tenta de novo se falhar). Condenar o
/// deploy `Live` nesse intervalo o deixava `Stopped` para sempre, mesmo com o container de volta, e o
/// `update_status(Stopped, None)` apagava o `live_container_id`.
const BOOT_GRACE: Duration = Duration::from_secs(10 * 60);

/// Num container parado, o `Live` deve ser poupado? Só quando a intenção é "no ar" **e** o daemon
/// acabou de subir: depois da janela, um container que sumiu de fato continua sendo corrigido.
fn spare_during_boot(desired: DesiredState, daemon_uptime: Duration) -> bool {
    desired == DesiredState::Running && daemon_uptime < BOOT_GRACE
}

/// Este deploy está `Stopped` por obra do reconcile (e não por um "Parar" de uma pessoa, que tem outra
/// mensagem)? Só esse pode voltar a `Live` quando o container volta.
fn is_reconcile_stop(dep: &Deployment) -> bool {
    dep.state == DeployState::Stopped
        && dep.states_log.last().and_then(|t| t.message.as_deref()) == Some(STOPPED_BY_RECONCILE)
}

/// Passa para Pruning/Stopped os deployments marcados Live que já não estão no
/// ar (só o mais recente de cada serviço pode ser Live), sem tocar em serviço
/// com deploy em curso.
pub async fn fix_stale_live(state: &AppState, deployments: Vec<Deployment>) -> Vec<Deployment> {
    // Collect the ID of the most-recent Live deployment per service.
    // The list is already ordered by started_at DESC, so the first Live entry
    // per service_id is the candidate; subsequent ones are stale.
    let mut seen_live: HashSet<String> = HashSet::new();
    // Serviços cujo deploy MAIS RECENTE (a lista vem do mais novo para o mais antigo) já foi visto.
    let mut seen_service: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(deployments.len());

    for mut dep in deployments {
        let is_newest = seen_service.insert(dep.service_id.clone());

        if dep.state != DeployState::Live {
            // O mais recente foi condenado por este reconcile (container ainda não tinha subido, ou
            // o `live_container_id` foi apagado) e o container voltou: o deploy volta a ser o Live.
            if is_newest
                && is_reconcile_stop(&dep)
                && !service_is_deploying(state, &dep.service_id).await
                && is_container_running(state, &dep.service_id).await
            {
                info!(
                    deployment_id = %dep.id,
                    service_id = %dep.service_id,
                    "reconcile: o container voltou — deployment Stopped pelo reconcile volta a Live"
                );
                transition_deployment(
                    state,
                    &mut dep,
                    DeployState::Live,
                    "container running again",
                )
                .await;
                seen_live.insert(dep.service_id.clone());
            }
            out.push(dep);
            continue;
        }

        // Deploy em andamento? Não toca em NADA desse serviço. O executor é a
        // fonte da verdade enquanto roda: ele mesmo promove o novo Live, poda o
        // antigo e escreve o status final (Running/Error).
        //
        // Sem esse guard o reconcile corrompia o estado durante todo deploy: ele
        // roda a cada snapshot (2s, via recent_deployments) e também no
        // deploy_history da aba Deployments, via o `is_container_running` abaixo
        // — que retorna false enquanto o container Live antigo está parado ou o
        // serviço ainda não subiu. Resultado: o Live anterior virava Stopped e o
        // status do serviço era reescrito para Stopped POR CIMA do Deploying que
        // o deploy_start tinha acabado de gravar. Na GUI: card "Stopped" com um
        // deployment "Building" logo abaixo.
        if service_is_deploying(state, &dep.service_id).await {
            out.push(dep);
            continue;
        }

        if seen_live.contains(&dep.service_id) {
            // A newer Live deployment was already accepted for this service.
            // This one is stale → Pruning.
            warn!(
                deployment_id = %dep.id,
                service_id = %dep.service_id,
                "reconcile: deployment Live duplicado para o mesmo serviço — corrigindo para Pruning"
            );
            transition_deployment(
                state,
                &mut dep,
                DeployState::Pruning,
                "superseded by newer deployment",
            )
            .await;
            out.push(dep);
            continue;
        }

        // First Live for this service: verify the container is actually running.
        if !is_container_running(state, &dep.service_id).await {
            // O deploy pode ter sido enfileirado entre o guard acima e a leitura dentro de
            // `is_container_running` (corrida de milissegundos, mas o reconcile roda a cada snapshot):
            // confere de novo antes de condenar o Live antigo.
            if service_is_deploying(state, &dep.service_id).await {
                out.push(dep);
                continue;
            }
            // Boot do daemon: o container ainda não foi religado pelo autostart. Poupa o Live (e o
            // `live_container_id`) em vez de condenar um serviço que está prestes a voltar.
            let desired = crate::store::services::desired_state(&state.db, &dep.service_id)
                .await
                .unwrap_or(DesiredState::Stopped);
            if spare_during_boot(desired, state.started_at.elapsed()) {
                seen_live.insert(dep.service_id.clone());
                out.push(dep);
                continue;
            }
            warn!(
                deployment_id = %dep.id,
                service_id = %dep.service_id,
                "reconcile: deployment Live mas container não está rodando — corrigindo para Stopped"
            );
            transition_deployment(state, &mut dep, DeployState::Stopped, STOPPED_BY_RECONCILE)
                .await;
            let _ = crate::store::services::update_status(
                &state.db,
                &dep.service_id,
                &ServiceStatus::Stopped,
                None,
            )
            .await;
            state.bus.publish(Event::ServiceStatusChanged {
                service_id: dep.service_id.clone(),
                status: ServiceStatus::Stopped,
            });
        } else {
            seen_live.insert(dep.service_id.clone());
        }

        out.push(dep);
    }

    out
}

/// True enquanto um deploy do serviço está em curso ou esperando na fila
/// (`ServiceStatus::Deploying`/`Queued`). `Queued` é escrito por `deploy_start`
/// ao enfileirar; `Deploying` pelo worker ao pegar o deploy — ambos limpos no
/// desfecho. Reconciliar por cima disso zeraria o status (o container ainda não
/// existe), então pulamos.
async fn service_is_deploying(state: &AppState, service_id: &str) -> bool {
    matches!(
        crate::store::services::get(&state.db, service_id).await,
        Ok(Some(svc)) if matches!(svc.status, ServiceStatus::Deploying | ServiceStatus::Queued)
    )
}

async fn transition_deployment(state: &AppState, dep: &mut Deployment, to: DeployState, msg: &str) {
    let _ = crate::store::deployments::transition(
        &state.db,
        &dep.id,
        &dep.state,
        to.clone(),
        Some(msg.into()),
    )
    .await;
    state.bus.publish(Event::DeployStateChanged {
        deployment_id: dep.id.clone(),
        service_id: dep.service_id.clone(),
        state: to.clone(),
        timestamp: Utc::now(),
        message: Some(msg.into()),
    });
    dep.state = to;
}

// Um deploy recém-promovido pode ter seu container ainda propagando o estado
// "running" no Docker Engine no instante exato em que este reconcile roda
// (ex.: GUI busca a lista de deployments quase junto com o evento DeployStateChanged
// de Live). Sem essa margem, esse read prematuro derruba um deploy que na
// prática subiu certo — visto em produção com um serviço Compose.
const RUNNING_CHECK_RETRIES: u32 = 3;
const RUNNING_CHECK_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

async fn is_container_running(state: &AppState, service_id: &str) -> bool {
    let svc = match crate::store::services::get(&state.db, service_id).await {
        Ok(Some(s)) => s,
        _ => return false,
    };

    if svc.status == ServiceStatus::Stopped {
        return false;
    }

    let Some(container_id) = svc.live_container_id else {
        return false;
    };

    for attempt in 0..RUNNING_CHECK_RETRIES {
        let running = match engine::containers::inspect(&state.docker.inner, &container_id).await {
            Ok(info) => info.state.as_ref().and_then(|s| s.running).unwrap_or(false),
            Err(_) => false,
        };
        if running {
            return true;
        }
        if attempt + 1 < RUNNING_CHECK_RETRIES {
            tokio::time::sleep(RUNNING_CHECK_DELAY).await;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(state: DeployState, last_message: Option<&str>) -> Deployment {
        serde_json::from_value(serde_json::json!({
            "id": "dep_1", "service_id": "svc_1", "image": "rp_x",
            "state": serde_json::to_value(&state).unwrap(),
            "started_at": "2026-11-07T00:00:00Z", "finished_at": null,
            "states_log": last_message.map(|m| vec![serde_json::json!({
                "from": "Live", "to": serde_json::to_value(&state).unwrap(),
                "at": "2026-11-07T00:00:00Z", "message": m,
            })]).unwrap_or_default(),
        }))
        .expect("Deployment de teste")
    }

    #[test]
    fn poupa_o_live_so_com_intencao_de_rodar_e_daemon_recem_subido() {
        let logo = Duration::from_secs(30);
        let depois = BOOT_GRACE + Duration::from_secs(1);
        assert!(spare_during_boot(DesiredState::Running, logo));
        assert!(
            !spare_during_boot(DesiredState::Running, depois),
            "passada a janela, um container que sumiu volta a ser corrigido"
        );
        assert!(
            !spare_during_boot(DesiredState::Stopped, logo),
            "serviço parado de propósito não é poupado"
        );
    }

    #[test]
    fn so_o_stopped_do_reconcile_pode_voltar_a_live() {
        assert!(is_reconcile_stop(&dep(
            DeployState::Stopped,
            Some(STOPPED_BY_RECONCILE)
        )));
        // um "Parar" de uma pessoa tem outra mensagem: não volta sozinho
        assert!(!is_reconcile_stop(&dep(
            DeployState::Stopped,
            Some("stopped by user")
        )));
        assert!(!is_reconcile_stop(&dep(DeployState::Stopped, None)));
        // e só vale para Stopped
        assert!(!is_reconcile_stop(&dep(
            DeployState::Failed,
            Some(STOPPED_BY_RECONCILE)
        )));
    }
}
