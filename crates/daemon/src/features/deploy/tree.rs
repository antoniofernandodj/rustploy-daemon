//! Deploy com a árvore de dependências (`Command::DeployWithDependencies` e `Command::ProjectDeploy`).
//!
//! O botão **Deploy** de um serviço e o **Deploy do projeto** enfileiram, **na ordem certa**, tudo o
//! que precisa estar no ar para o serviço (ou o projeto) funcionar: primeiro quem não depende de
//! ninguém (um banco, por exemplo), depois quem depende deles, e assim por diante. É o mesmo grafo do
//! religamento do boot (`autostart`): o `depends_on` do serviço mais as arestas implícitas do banco
//! compartilhado (`shared_access`).
//!
//! A fila de deploys tem **uma vaga só** e é FIFO, então a ordem de entrada é a ordem de execução, e
//! cada deploy termina (no ar ou falho) antes do seguinte começar. Falta só impedir que quem depende
//! de uma dependência **que falhou** rode mesmo assim: cada deploy do lote guarda de quais deployments
//! ele depende (`DeployQueue::set_waits`) e `queue::run_one` o recusa se algum não terminou bem.
//!
//! Lógica de ordenação pura em [`plan`] (testável sem banco nem Docker).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use shared::{
    DeployBatchItem, DeployBatchStatus, DeployState, Event, Response as RpResponse, Service,
    ServiceStatus,
};
use tracing::{info, warn};

use crate::features::deploy::{DeployContext, deps};

/// Um passo do plano: um serviço, a onda dele na árvore e de quais outros passos ele depende.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub id: String,
    pub name: String,
    pub wave: u32,
    /// Passos (ids) de que este depende **dentro do plano**, com ou sem `skip`.
    pub depends_on: Vec<String>,
    /// `Some(motivo)` = este serviço não entra na fila (e quem depende dele não o espera).
    pub skip: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PlanError {
    /// O serviço (ou projeto) pedido não existe.
    NotFound(String),
    /// A árvore tem um ciclo: nomes dos serviços envolvidos.
    Cycle(Vec<String>),
}

/// Monta o plano para `targets`: eles e **todas** as suas dependências (diretas e indiretas), em ordem de
/// onda. `implicit` são as arestas `(dependente, dependência)` do banco compartilhado.
///
/// Com `skip_running`, uma **dependência** que já está `Running` não entra na fila (um alvo entra sempre).
/// Um serviço que já está na fila ou em deploy também é pulado (um segundo deploy seria recusado).
pub fn plan(
    services: &[Service],
    implicit: &[(String, String)],
    targets: &[String],
    skip_running: bool,
) -> Result<Vec<PlanStep>, PlanError> {
    let by_id: BTreeMap<&str, &Service> = services.iter().map(|s| (s.id.as_str(), s)).collect();
    for t in targets {
        if !by_id.contains_key(t.as_str()) {
            return Err(PlanError::NotFound(t.clone()));
        }
    }

    // Grafo completo: o `depends_on` de cada serviço + as arestas implícitas (entre serviços que existem).
    let mut graph: deps::Graph = by_id
        .iter()
        .map(|(id, s)| {
            (
                (*id).to_string(),
                s.spec
                    .depends_on
                    .iter()
                    .filter(|d| by_id.contains_key(d.as_str()) && d.as_str() != *id)
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect();
    for (dependent, dependency) in implicit {
        if dependent != dependency
            && by_id.contains_key(dependent.as_str())
            && by_id.contains_key(dependency.as_str())
        {
            graph
                .entry(dependent.clone())
                .or_default()
                .insert(dependency.clone());
        }
    }

    // Fecho transitivo: os alvos e tudo de que eles dependem.
    let mut needed: BTreeSet<String> = BTreeSet::new();
    let mut frontier: Vec<String> = targets.to_vec();
    while let Some(id) = frontier.pop() {
        if needed.insert(id.clone())
            && let Some(deps_of) = graph.get(&id)
        {
            frontier.extend(deps_of.iter().cloned());
        }
    }
    let sub: deps::Graph = needed
        .iter()
        .map(|id| {
            (
                id.clone(),
                graph[id]
                    .iter()
                    .filter(|d| needed.contains(*d))
                    .cloned()
                    .collect(),
            )
        })
        .collect();

    let waves = deps::waves(&sub).map_err(|cycle| {
        PlanError::Cycle(
            cycle
                .iter()
                .map(|id| {
                    by_id
                        .get(id.as_str())
                        .map_or(id.clone(), |s| s.spec.name.clone())
                })
                .collect(),
        )
    })?;

    let target_set: BTreeSet<&str> = targets.iter().map(String::as_str).collect();
    let mut steps = Vec::new();
    for (wave, ids) in waves.iter().enumerate() {
        for id in ids {
            let svc = by_id[id.as_str()];
            let skip = if matches!(svc.status, ServiceStatus::Deploying | ServiceStatus::Queued) {
                Some("já está na fila ou em deploy".to_string())
            } else if skip_running
                && !target_set.contains(id.as_str())
                && svc.status == ServiceStatus::Running
            {
                Some("já está no ar".to_string())
            } else {
                None
            };
            steps.push(PlanStep {
                id: id.clone(),
                name: svc.spec.name.clone(),
                wave: wave as u32,
                depends_on: sub[id].iter().cloned().collect(),
                skip,
            });
        }
    }
    Ok(steps)
}

/// Enfileira o plano, na ordem. Um serviço cuja dependência não pôde ser enfileirada **não** é
/// enfileirado (`Blocked`); o deploy de quem depende de um deploy que falhar depois é barrado pela
/// trava de `queue::run_one`.
pub async fn run_plan(ctx: &DeployContext, steps: &[PlanStep]) -> Vec<DeployBatchItem> {
    let mut items = Vec::with_capacity(steps.len());
    // serviço → deployment criado neste lote
    let mut deployed: HashMap<String, String> = HashMap::new();
    // serviços que não puderam entrar na fila (o que depende deles fica bloqueado)
    let mut broken: BTreeMap<String, String> = BTreeMap::new();

    for step in steps {
        let mut item = DeployBatchItem {
            service_id: step.id.clone(),
            service_name: step.name.clone(),
            wave: step.wave,
            status: DeployBatchStatus::Queued,
            deployment_id: None,
            message: None,
        };
        if let Some(motivo) = &step.skip {
            item.status = DeployBatchStatus::Skipped;
            item.message = Some(motivo.clone());
            items.push(item);
            continue;
        }
        if let Some(nome) = step.depends_on.iter().find_map(|d| broken.get(d).cloned()) {
            item.status = DeployBatchStatus::Blocked;
            item.message = Some(format!("a dependência '{nome}' não pôde ser enfileirada"));
            broken.insert(step.id.clone(), step.name.clone());
            items.push(item);
            continue;
        }

        let waits: Vec<String> = step
            .depends_on
            .iter()
            .filter_map(|d| deployed.get(d).cloned())
            .collect();
        match crate::features::deploy::start::handle_with_waits(ctx.clone(), step.id.clone(), waits)
            .await
        {
            RpResponse::Deployment(d) => {
                deployed.insert(step.id.clone(), d.id.clone());
                item.deployment_id = Some(d.id);
            }
            RpResponse::Err { code, .. } if code == "ServiceAlreadyDeploying" => {
                // Entrou na fila entre o plano e agora: não é falha.
                item.status = DeployBatchStatus::Skipped;
                item.message = Some("já está na fila ou em deploy".into());
            }
            RpResponse::Err { message, .. } => {
                item.status = DeployBatchStatus::Failed;
                item.message = Some(message);
                broken.insert(step.id.clone(), step.name.clone());
            }
            outra => {
                item.status = DeployBatchStatus::Failed;
                item.message = Some(format!("resposta inesperada: {outra:?}"));
                broken.insert(step.id.clone(), step.name.clone());
            }
        }
        items.push(item);
    }
    items
}

/// Carrega o necessário do banco e devolve o plano para `targets`.
async fn plan_from_db(
    ctx: &DeployContext,
    targets: &[String],
    skip_running: bool,
) -> Result<Vec<PlanStep>, RpResponse> {
    let all = crate::store::services::list_all(&ctx.db)
        .await
        .map_err(|e| RpResponse::err("DatabaseError", e.to_string()))?;
    let access = crate::store::shared_access::all(&ctx.db)
        .await
        .unwrap_or_default();
    let implicit = crate::features::deploy::autostart::implicit_edges(&access, &all);
    plan(&all, &implicit, targets, skip_running).map_err(|e| match e {
        PlanError::NotFound(id) => {
            RpResponse::err("NotFound", format!("serviço {id} não encontrado"))
        }
        PlanError::Cycle(nomes) => RpResponse::err(
            "DependencyCycle",
            format!(
                "as dependências formam um ciclo e não têm ordem possível: {}",
                nomes.join(", ")
            ),
        ),
    })
}

/// O plano como a resposta (`DeployBatch`), **sem enfileirar**: `Queued` = entraria na fila.
fn plan_response(steps: &[PlanStep]) -> RpResponse {
    RpResponse::DeployBatch(
        steps
            .iter()
            .map(|s| DeployBatchItem {
                service_id: s.id.clone(),
                service_name: s.name.clone(),
                wave: s.wave,
                status: if s.skip.is_some() {
                    DeployBatchStatus::Skipped
                } else {
                    DeployBatchStatus::Queued
                },
                deployment_id: None,
                message: s.skip.clone(),
            })
            .collect(),
    )
}

/// `Command::DeployWithDependenciesPlan`: o que o `DeployWithDependencies` faria, sem fazer.
pub async fn plan_for_service(
    ctx: DeployContext,
    service_id: String,
    skip_running: bool,
) -> RpResponse {
    match plan_from_db(&ctx, std::slice::from_ref(&service_id), skip_running).await {
        Ok(steps) => plan_response(&steps),
        Err(resp) => resp,
    }
}

/// `Command::ProjectDeployPlan`: o que o `ProjectDeploy` faria, sem fazer.
pub async fn plan_for_project(
    ctx: DeployContext,
    project_id: String,
    skip_running: bool,
) -> RpResponse {
    let ids: Vec<String> = match crate::store::services::list(&ctx.db, &project_id).await {
        Ok(v) => v.into_iter().map(|s| s.id).collect(),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    if ids.is_empty() {
        return RpResponse::err("NotFound", "o projeto não tem serviços (ou não existe)");
    }
    match plan_from_db(&ctx, &ids, skip_running).await {
        Ok(steps) => plan_response(&steps),
        Err(resp) => resp,
    }
}

/// `Command::DeployWithDependencies`.
pub async fn deploy_with_dependencies(
    ctx: DeployContext,
    service_id: String,
    skip_running: bool,
) -> RpResponse {
    let steps = match plan_from_db(&ctx, std::slice::from_ref(&service_id), skip_running).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    info!(service_id = %service_id, passos = steps.len(), "deploy em lote: serviço com dependências");
    RpResponse::DeployBatch(run_plan(&ctx, &steps).await)
}

/// `Command::ProjectDeploy`.
pub async fn project_deploy(
    ctx: DeployContext,
    project_id: String,
    skip_running: bool,
) -> RpResponse {
    let ids: Vec<String> = match crate::store::services::list(&ctx.db, &project_id).await {
        Ok(v) => v.into_iter().map(|s| s.id).collect(),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    if ids.is_empty() {
        return RpResponse::err("NotFound", "o projeto não tem serviços (ou não existe)");
    }
    let steps = match plan_from_db(&ctx, &ids, skip_running).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    info!(project_id = %project_id, passos = steps.len(), "deploy em lote: projeto inteiro");
    RpResponse::DeployBatch(run_plan(&ctx, &steps).await)
}

// ── A trava: quem depende de uma dependência que falhou não roda ─────────────────────────────────

/// Algum dos deployments de que o deploy depende terminou **mal** (falhou ou foi interrompido)? Devolve o
/// motivo para a mensagem. Os deploys de um lote rodam em ordem na vaga única, então, quando este chega, os
/// de que ele depende já terminaram.
pub async fn failed_dependency(db: &crate::store::Db, waits: &[String]) -> Option<String> {
    for id in waits {
        let Ok(Some(dep)) = crate::store::deployments::get(db, id).await else {
            continue;
        };
        if matches!(dep.state, DeployState::Failed | DeployState::Stopped) {
            let nome = match crate::store::services::get(db, &dep.service_id).await {
                Ok(Some(s)) => s.spec.name,
                _ => dep.service_id.clone(),
            };
            return Some(format!(
                "a dependência '{nome}' não foi implantada (deploy {})",
                dep.state.label()
            ));
        }
    }
    None
}

/// Não roda o deploy `dep_id`: o marca como `Failed` com o `motivo` e devolve o serviço ao estado que
/// ele tinha (no ar se ainda tem container, senão com o erro).
pub async fn skip_for_dependency(state: &DeployContext, dep_id: &str, motivo: &str) {
    let Ok(Some(dep)) = crate::store::deployments::get(&state.db, dep_id).await else {
        return;
    };
    warn!(deployment_id = %dep_id, motivo, "deploy em lote: dependência falhou, este deploy não roda");
    let _ = crate::store::deployments::transition(
        &state.db,
        dep_id,
        &dep.state,
        DeployState::Failed,
        Some(motivo.to_string()),
    )
    .await;
    state.bus.publish(Event::DeployStateChanged {
        deployment_id: dep_id.to_string(),
        service_id: dep.service_id.clone(),
        state: DeployState::Failed,
        timestamp: chrono::Utc::now(),
        message: Some(motivo.to_string()),
    });
    if let Ok(Some(svc)) = crate::store::services::get(&state.db, &dep.service_id).await {
        let status = if svc.live_container_id.is_some() {
            ServiceStatus::Running
        } else {
            ServiceStatus::Error(format!("não implantado: {motivo}"))
        };
        let _ = crate::store::services::update_status(
            &state.db,
            &dep.service_id,
            &status,
            svc.live_container_id.as_deref(),
        )
        .await;
        state.bus.publish(Event::ServiceStatusChanged {
            service_id: dep.service_id,
            status,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(id: &str, deps: &[&str], status: ServiceStatus) -> Service {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "spec": {
                "name": id, "project_id": "prj", "port": 80,
                "source": { "Registry": { "image": "x" } },
                "env_vars": [], "volumes": [], "domain": null, "host_port": null,
                "tls_enabled": false, "replicas": 1,
                "healthcheck": { "kind": "None", "interval_secs": 5, "timeout_secs": 3, "retries": 3, "start_period_secs": 0 },
                "resources": { "cpu_shares": 0, "mem_limit_bytes": 0 },
                "depends_on": deps,
            },
            "status": serde_json::to_value(&status).unwrap(),
            "live_container_id": null,
            "created_at": "2026-11-08T00:00:00Z",
            "updated_at": "2026-11-08T00:00:00Z",
        }))
        .expect("Service de teste")
    }

    fn ids(steps: &[PlanStep]) -> Vec<&str> {
        steps.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn banco_antes_da_api_antes_do_front() {
        // front → api → (db, cache): o front depende da api; a api, do banco e do cache.
        let s = vec![
            svc("front", &["api"], ServiceStatus::Stopped),
            svc("api", &["db", "cache"], ServiceStatus::Stopped),
            svc("db", &[], ServiceStatus::Stopped),
            svc("cache", &[], ServiceStatus::Stopped),
            svc("outro", &[], ServiceStatus::Stopped), // fora da árvore do front
        ];
        let p = plan(&s, &[], &["front".into()], false).unwrap();
        assert_eq!(ids(&p), ["cache", "db", "api", "front"]);
        assert_eq!(p.iter().map(|x| x.wave).collect::<Vec<_>>(), [0, 0, 1, 2]);
        assert!(
            !ids(&p).contains(&"outro"),
            "o que não está na árvore fica de fora"
        );
        let api = p.iter().find(|x| x.id == "api").unwrap();
        assert_eq!(api.depends_on, ["cache", "db"]);
    }

    #[test]
    fn dependencia_indireta_e_diamante_entram_uma_vez_so() {
        // a → b, a → c, b → d, c → d (diamante)
        let s = vec![
            svc("a", &["b", "c"], ServiceStatus::Stopped),
            svc("b", &["d"], ServiceStatus::Stopped),
            svc("c", &["d"], ServiceStatus::Stopped),
            svc("d", &[], ServiceStatus::Stopped),
        ];
        let p = plan(&s, &[], &["a".into()], false).unwrap();
        assert_eq!(ids(&p), ["d", "b", "c", "a"]);
    }

    #[test]
    fn o_projeto_inteiro_sai_na_ordem_para_todos_funcionarem() {
        let s = vec![
            svc("front", &["api"], ServiceStatus::Stopped),
            svc("api", &["db"], ServiceStatus::Stopped),
            svc("db", &[], ServiceStatus::Stopped),
            svc("worker", &["db"], ServiceStatus::Stopped),
        ];
        let alvos: Vec<String> = s.iter().map(|x| x.id.clone()).collect();
        let p = plan(&s, &[], &alvos, false).unwrap();
        let pos = |id: &str| ids(&p).iter().position(|x| *x == id).unwrap();
        assert!(pos("db") < pos("api") && pos("db") < pos("worker"));
        assert!(pos("api") < pos("front"));
        assert_eq!(p.len(), 4);
    }

    #[test]
    fn skip_running_pula_dependencia_no_ar_mas_nunca_o_alvo() {
        let s = vec![
            svc("api", &["db"], ServiceStatus::Running),
            svc("db", &[], ServiceStatus::Running),
        ];
        let p = plan(&s, &[], &["api".into()], true).unwrap();
        let db = p.iter().find(|x| x.id == "db").unwrap();
        let api = p.iter().find(|x| x.id == "api").unwrap();
        assert_eq!(db.skip.as_deref(), Some("já está no ar"));
        assert_eq!(api.skip, None, "o serviço pedido sempre entra");
        // sem skip_running, tudo entra
        let p = plan(&s, &[], &["api".into()], false).unwrap();
        assert!(p.iter().all(|x| x.skip.is_none()));
    }

    #[test]
    fn servico_ja_na_fila_ou_em_deploy_e_pulado() {
        let s = vec![
            svc("api", &["db"], ServiceStatus::Stopped),
            svc("db", &[], ServiceStatus::Queued),
        ];
        let p = plan(&s, &[], &["api".into()], false).unwrap();
        assert_eq!(
            p.iter().find(|x| x.id == "db").unwrap().skip.as_deref(),
            Some("já está na fila ou em deploy")
        );
    }

    #[test]
    fn banco_compartilhado_entra_como_dependencia_implicita() {
        // `srv` é um servidor de banco compartilhado; o projeto de `app` tem acesso a ele.
        let s = vec![
            svc("app", &[], ServiceStatus::Stopped),
            svc("srv", &[], ServiceStatus::Stopped),
        ];
        let implicito = vec![("app".to_string(), "srv".to_string())];
        let p = plan(&s, &implicito, &["app".into()], false).unwrap();
        assert_eq!(ids(&p), ["srv", "app"]);
    }

    #[test]
    fn ciclo_e_servico_inexistente_sao_erro() {
        let s = vec![
            svc("a", &["b"], ServiceStatus::Stopped),
            svc("b", &["a"], ServiceStatus::Stopped),
        ];
        match plan(&s, &[], &["a".into()], false) {
            Err(PlanError::Cycle(nomes)) => assert_eq!(nomes.len(), 2),
            outro => panic!("esperava ciclo, veio {outro:?}"),
        }
        assert_eq!(
            plan(&s, &[], &["nada".into()], false),
            Err(PlanError::NotFound("nada".into()))
        );
    }

    #[test]
    fn dependencia_que_nao_existe_mais_e_ignorada() {
        // depends_on aponta para um serviço apagado: não trava o plano
        let s = vec![svc("a", &["fantasma"], ServiceStatus::Stopped)];
        let p = plan(&s, &[], &["a".into()], false).unwrap();
        assert_eq!(ids(&p), ["a"]);
    }
}
