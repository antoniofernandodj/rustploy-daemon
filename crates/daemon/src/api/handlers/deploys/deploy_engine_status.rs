//! `Command::DeployEngineStatus`: estado da tela Deploy Engine: fila (na ordem
//! da fila), o que está rodando, pausa e histórico recente, com nomes de
//! serviço/projeto.

use crate::bootstrap::state::AppState;
use chrono::Utc;
use shared::{
    ActiveDeployInfo, DeployEngineSummary, DeployState, QueueItemKind, Response as RpResponse,
};
use std::collections::HashMap;

/// Um item que não é deploy (a execução de um job ou o aquecimento de um) no formato das listas do
/// Deploy Engine. `deployment_id` leva o id do item na fila (`jrun_…`/`jwarm_…`), que serve a
/// promover, reordenar e cancelar; `service_name` é o nome do job. `None` se o job foi apagado.
#[allow(clippy::too_many_arguments)]
async fn job_item(
    state: &AppState,
    kind: QueueItemKind,
    item_id: &str,
    job_id: &str,
    deploy_state: DeployState,
    started_at: chrono::DateTime<Utc>,
    elapsed_secs: u64,
    current_state_secs: u64,
) -> Option<ActiveDeployInfo> {
    let job = crate::store::job::get(&state.db, job_id).await.ok()??;
    let project_name = match crate::store::projects::get(&state.db, &job.project_id).await {
        Ok(Some(p)) => p.name,
        _ => job.project_id.clone(),
    };
    Some(ActiveDeployInfo {
        deployment_id: item_id.to_string(),
        service_id: job.trigger_service_id.clone().unwrap_or_default(),
        service_name: job.name,
        project_name,
        state: deploy_state,
        started_at,
        elapsed_secs,
        current_state_secs,
        states: Vec::new(),
        kind,
    })
}

/// Quando o item foi criado: a execução de job tem linha no banco; o aquecimento só tem o ULID do
/// id (`jwarm_<ulid>`), que carrega a hora.
async fn item_started_at(state: &AppState, kind: QueueItemKind, id: &str) -> chrono::DateTime<Utc> {
    if kind == QueueItemKind::Job
        && let Ok(Some(run)) = crate::store::job_run::get(&state.db, id).await
    {
        return run.started_at;
    }
    id.split_once('_')
        .and_then(|(_, ulid)| ulid::Ulid::from_string(ulid).ok())
        .map(|u| chrono::DateTime::<Utc>::from(u.datetime()))
        .unwrap_or_else(Utc::now)
}

pub async fn handle(state: AppState) -> RpResponse {
    let db = &state.db;

    let active_deps = match crate::store::deployments::get_non_terminal(db).await {
        Ok(d) => d,
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };

    let recent_deps = match crate::store::deployments::list_terminal_last_24h(db, 20).await {
        Ok(d) => d,
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };

    let (total_24h, successful_24h, failed_24h) = crate::store::deployments::stats_last_24h(db)
        .await
        .unwrap_or_default();

    // Cache service+project name lookups.
    let mut svc_cache: HashMap<String, (String, String)> = HashMap::new();
    let mut proj_cache: HashMap<String, String> = HashMap::new();

    let resolve = |svc_cache: &mut HashMap<String, (String, String)>,
                   _proj_cache: &mut HashMap<String, String>,
                   service_id: &str|
     -> (String, String) {
        if let Some(cached) = svc_cache.get(service_id) {
            return cached.clone();
        }
        (service_id.to_string(), String::new())
    };
    let _ = resolve; // avoid unused warning — we resolve inline below

    async fn resolve_names(
        state: &AppState,
        service_id: &str,
        svc_cache: &mut HashMap<String, (String, String)>,
        proj_cache: &mut HashMap<String, String>,
    ) -> (String, String) {
        if let Some(cached) = svc_cache.get(service_id) {
            return cached.clone();
        }
        let (sname, pid) = match crate::store::services::get(&state.db, service_id).await {
            Ok(Some(s)) => (s.spec.name, s.spec.project_id),
            _ => (service_id.to_string(), String::new()),
        };
        let pname = if pid.is_empty() {
            String::new()
        } else if let Some(p) = proj_cache.get(&pid) {
            p.clone()
        } else {
            let name = match crate::store::projects::get(&state.db, &pid).await {
                Ok(Some(p)) => p.name,
                _ => pid.clone(),
            };
            proj_cache.insert(pid.clone(), name.clone());
            name
        };
        svc_cache.insert(service_id.to_string(), (sname.clone(), pname.clone()));
        (sname, pname)
    }

    let now = Utc::now();

    let mut active = Vec::with_capacity(active_deps.len());
    for dep in active_deps {
        let (service_name, project_name) =
            resolve_names(&state, &dep.service_id, &mut svc_cache, &mut proj_cache).await;

        let elapsed_secs = (now - dep.started_at).num_seconds().max(0) as u64;

        let current_state_secs = dep
            .states_log
            .last()
            .map(|t| (now - t.at).num_seconds().max(0) as u64)
            .unwrap_or(elapsed_secs);

        active.push(ActiveDeployInfo {
            deployment_id: dep.id,
            service_id: dep.service_id,
            service_name,
            project_name,
            state: dep.state,
            started_at: dep.started_at,
            elapsed_secs,
            current_state_secs,
            states: dep.states_log.clone(),
            kind: QueueItemKind::Deploy,
        });
    }

    let mut recent = Vec::with_capacity(recent_deps.len());
    for dep in recent_deps {
        let (service_name, project_name) =
            resolve_names(&state, &dep.service_id, &mut svc_cache, &mut proj_cache).await;

        let elapsed_secs = dep
            .finished_at
            .map(|f| (f - dep.started_at).num_seconds().max(0) as u64)
            .unwrap_or_else(|| (now - dep.started_at).num_seconds().max(0) as u64);

        let current_state_secs = (now - dep.started_at).num_seconds().max(0) as u64;

        recent.push(ActiveDeployInfo {
            deployment_id: dep.id,
            service_id: dep.service_id,
            service_name,
            project_name,
            state: dep.state,
            started_at: dep.started_at,
            elapsed_secs,
            current_state_secs,
            states: dep.states_log.clone(),
            kind: QueueItemKind::Deploy,
        });
    }

    // Execuções de job terminadas entram no mesmo histórico, intercaladas por horário.
    if let Ok(runs) = crate::store::job_run::list_finished_last_24h(db, 20).await {
        for run in runs {
            let outcome = if run.success == Some(true) {
                DeployState::Live
            } else if run.exit_code == Some(crate::engine::compose::CANCELLED_EXIT_CODE) {
                DeployState::Stopped
            } else {
                DeployState::Failed
            };
            let elapsed = run
                .finished_at
                .map(|f| (f - run.started_at).num_seconds().max(0) as u64)
                .unwrap_or(0);
            if let Some(info) = job_item(
                &state,
                QueueItemKind::Job,
                &run.id,
                &run.job_id,
                outcome,
                run.started_at,
                elapsed,
                elapsed,
            )
            .await
            {
                recent.push(info);
            }
        }
        recent.sort_by_key(|item| std::cmp::Reverse(item.started_at));
        recent.truncate(20);
    }

    // Separa os que estão só esperando na fila (Pending, presentes na
    // DeployQueue) dos que realmente estão em execução. `queued` sai na ordem da
    // fila (o primeiro é o próximo a rodar), não na ordem do banco.
    let (running_id, queued_ids, paused) = state.deploy_queue.snapshot();
    let queued_set: std::collections::HashSet<&str> =
        queued_ids.iter().map(|s| s.as_str()).collect();

    let mut running_active = Vec::with_capacity(active.len());
    let mut queued_by_id: HashMap<String, ActiveDeployInfo> = HashMap::new();
    for info in active {
        if queued_set.contains(info.deployment_id.as_str()) {
            queued_by_id.insert(info.deployment_id.clone(), info);
        } else {
            running_active.push(info);
        }
    }

    // Jobs e aquecimentos: itens da mesma fila. Esperando → "Na fila" (na posição deles);
    // o que está com a vaga agora → "Executando".
    let now = Utc::now();
    for (id, task) in state.deploy_queue.task_infos() {
        let is_running = running_id.as_deref() == Some(id.as_str());
        if !is_running && !queued_set.contains(id.as_str()) {
            continue;
        }
        let started_at = item_started_at(&state, task.kind, &id).await;
        let elapsed = (now - started_at).num_seconds().max(0) as u64;
        let deploy_state = if is_running {
            DeployState::ComposingUp
        } else {
            DeployState::Pending
        };
        let Some(info) = job_item(
            &state,
            task.kind,
            &id,
            &task.job_id,
            deploy_state,
            started_at,
            elapsed,
            elapsed,
        )
        .await
        else {
            continue;
        };
        if is_running {
            running_active.push(info);
        } else {
            queued_by_id.insert(id, info);
        }
    }

    let queued: Vec<ActiveDeployInfo> = queued_ids
        .iter()
        .filter_map(|id| queued_by_id.remove(id))
        .collect();
    let active = running_active;

    RpResponse::DeployEngineStatus(Box::new(DeployEngineSummary {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        active,
        recent,
        total_24h,
        successful_24h,
        failed_24h,
        queued,
        paused,
    }))
}
