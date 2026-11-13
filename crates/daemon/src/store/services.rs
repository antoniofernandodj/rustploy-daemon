//! Serviços: o `ServiceSpec` e o nome da stack Compose moram no **YAML de infra**
//! (`store::infra`); o **runtime** (status, container live, intenção `desired_state`) mora no
//! SQLite, na tabela `service_runtime`. Plano: `docs/plano-backup-estado-em-git.md`, Fases 2 e 4d.
//!
//! A tabela antiga `service` não é mais lida nem escrita (fica como rollback).

use super::Db;
use super::infra::model::{InfraState, ServiceInfra};
use anyhow::Result;
use chrono::{DateTime, Utc};
use shared::{Service, ServiceSource, ServiceSpec, ServiceStatus};
use std::collections::HashMap;
use tracing::{debug, info, warn};
use ulid::Ulid;

/// Linha de `service_runtime`: `(status, live_container_id, desired_state, updated_at)`.
type Runtime = (String, Option<String>, String, DateTime<Utc>);
type RuntimeRow = (String, String, Option<String>, String, DateTime<Utc>);

async fn runtimes(db: &Db) -> Result<HashMap<String, Runtime>> {
    let rows: Vec<RuntimeRow> = sqlx::query_as(
        "SELECT service_id, status, live_container_id, desired_state, updated_at \
         FROM service_runtime",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, st, c, d, u)| (id, (st, c, d, u)))
        .collect())
}

fn parse_status(s: &str) -> ServiceStatus {
    match s {
        "Stopped" => ServiceStatus::Stopped,
        "Stopping" => ServiceStatus::Stopping,
        "Deploying" => ServiceStatus::Deploying,
        "Queued" => ServiceStatus::Queued,
        "Running" => ServiceStatus::Running,
        "Degraded" => ServiceStatus::Degraded,
        s if s.starts_with("Error:") => {
            ServiceStatus::Error(s.trim_start_matches("Error:").trim().to_string())
        }
        _ => ServiceStatus::Stopped,
    }
}

/// Junta a parte declarada (YAML) com o runtime (SQL). Serviço sem linha de runtime lê como
/// `Stopped`.
fn assemble(s: &ServiceInfra, rt: Option<&Runtime>) -> Service {
    Service {
        id: s.id.clone(),
        spec: s.spec.clone(),
        compose_project: s.compose_project.clone(),
        status: rt
            .map(|r| parse_status(&r.0))
            .unwrap_or(ServiceStatus::Stopped),
        live_container_id: rt.and_then(|r| r.1.clone()),
        created_at: s.created_at,
        updated_at: rt.map(|r| r.3).unwrap_or(s.created_at),
    }
}

/// Todos os serviços do YAML, com o runtime junto: projeto por projeto, e dentro de cada um na ordem
/// de exibição (`service_order`, depois criação).
async fn all_services(db: &Db) -> Result<Vec<(Service, Option<Runtime>)>> {
    let rts = runtimes(db).await?;
    let st = db.infra().snapshot();
    let out: Vec<(Service, Option<Runtime>)> = st
        .projects
        .iter()
        .flat_map(|p| p.services_in_order())
        .map(|s| {
            let rt = rts.get(&s.id).cloned();
            (assemble(s, rt.as_ref()), rt)
        })
        .collect();
    Ok(out)
}

async fn filtered(db: &Db, keep: impl Fn(&Runtime) -> bool) -> Result<Vec<Service>> {
    Ok(all_services(db)
        .await?
        .into_iter()
        .filter(|(_, rt)| rt.as_ref().is_some_and(&keep))
        .map(|(s, _)| s)
        .collect())
}

/// Nome de stack para um serviço Compose novo: o curto
/// (`rp_<últimos 8 do ID>_<nome>`); se já está em uso — chance de ~1 em 10¹² —,
/// o ID inteiro.
fn new_compose_project(st: &InfraState, id: &str, name: &str) -> String {
    let short = shared::new_compose_project_name(id, name);
    let taken = st
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
        .any(|s| s.compose_project.as_deref() == Some(short.as_str()));
    if taken {
        shared::new_compose_project_name_long(id, name)
    } else {
        short
    }
}

/// Regras de nome/`shared` (prefixo `rp-shared-` reservado etc.) — ver
/// `crate::platform::connection::validate_shared_rules`.
fn check_shared_rules(spec: &ServiceSpec) -> Result<()> {
    crate::platform::connection::validate_shared_rules(
        &spec.name,
        &spec.source,
        spec.product.as_deref(),
        spec.shared.is_some(),
    )
    .map_err(|e| anyhow::anyhow!(e))
}

pub async fn create(db: &Db, spec: ServiceSpec) -> Result<Service> {
    check_shared_rules(&spec)?;
    let id = format!("svc_{}", Ulid::new());
    info!(id = %id, name = %spec.name, project_id = %spec.project_id, "db::services::create");
    let now = Utc::now();
    let new_safe = spec.safe_name();
    let rec = db.infra().update(|st| {
        // Unicidade: dois serviços não podem ter o mesmo nome (normalizado) no mesmo projeto —
        // o nome vira o container/DNS `rp_<safe_name>` e o compose project name, então
        // colidiriam. Compara por `normalize_name` para pegar também nomes diferentes que
        // colapsam no mesmo safe_name (ex.: "my-api" e "my_api").
        let compose_project = match &spec.source {
            ServiceSource::Compose(_) => Some(new_compose_project(st, &id, &spec.name)),
            _ => None,
        };
        let Some(project) = st.projects.iter_mut().find(|p| p.id == spec.project_id) else {
            anyhow::bail!("projeto {} não encontrado", spec.project_id);
        };
        if project
            .services
            .iter()
            .any(|s| shared::normalize_name(&s.spec.name) == new_safe)
        {
            anyhow::bail!(
                "já existe um serviço com o nome \"{}\" neste projeto",
                spec.name
            );
        }
        let rec = ServiceInfra {
            id: id.clone(),
            spec: spec.clone(),
            compose_project,
            created_at: now,
            webhook_token: None,
            secrets: vec![],
            backup: None,
        };
        project.services.push(rec.clone());
        Ok(rec)
    })?;
    // O runtime é opcional (sem linha lê como Stopped): se esta gravação falhar, o serviço já
    // existe no YAML e nada fica inconsistente.
    sqlx::query(
        "INSERT OR IGNORE INTO service_runtime (service_id, status, live_container_id, desired_state, updated_at)
         VALUES (?, 'Stopped', NULL, 'Stopped', ?)",
    )
    .bind(&id)
    .bind(now)
    .execute(db.pool())
    .await?;
    let svc = assemble(&rec, None);
    info!(service_id = %svc.id, name = %svc.spec.name, "db::services::create: salvo");
    Ok(svc)
}

/// Define a ordem de exibição dos serviços de um projeto: `ids` na ordem desejada; os que não
/// vierem ficam no fim, na ordem em que já eram mostrados. `Err(motivo)` (aninhado) para id
/// repetido, de outro projeto, ou projeto inexistente — e nada muda.
pub async fn reorder(
    db: &Db,
    project_id: &str,
    ids: &[String],
) -> Result<std::result::Result<(), String>> {
    db.infra().update(|st| {
        let Some(project) = st.projects.iter_mut().find(|p| p.id == project_id) else {
            return Ok(Err("project not found".to_string()));
        };
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            if !project.services.iter().any(|s| &s.id == id) {
                return Ok(Err(format!("serviço {id} não é deste projeto")));
            }
            if !seen.insert(id.as_str()) {
                return Ok(Err(format!("serviço {id} repetido")));
            }
        }
        let rest: Vec<String> = project
            .services_in_order()
            .into_iter()
            .map(|s| s.id.clone())
            .filter(|id| !ids.contains(id))
            .collect();
        project.service_order = ids.iter().cloned().chain(rest).collect();
        Ok(Ok(()))
    })
}

pub async fn list(db: &Db, project_id: &str) -> Result<Vec<Service>> {
    Ok(all_services(db)
        .await?
        .into_iter()
        .map(|(s, _)| s)
        .filter(|s| s.spec.project_id == project_id)
        .collect())
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Service>> {
    let st = db.infra().snapshot();
    let Some(rec) = st
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
        .find(|s| s.id == id)
    else {
        return Ok(None);
    };
    let rt = runtimes(db).await?.remove(id);
    Ok(Some(assemble(rec, rt.as_ref())))
}

pub async fn update_spec(db: &Db, id: &str, spec: ServiceSpec) -> Result<Option<Service>> {
    check_shared_rules(&spec)?;
    // Mesma regra de unicidade de `create`, mas ignorando o próprio serviço: um rename não pode
    // colidir (por nome normalizado) com outro serviço do mesmo projeto.
    let new_safe = spec.safe_name();
    let found = db.infra().update(|st| {
        let compose_name = new_compose_project(st, id, &spec.name);
        let Some(project) = st
            .projects
            .iter_mut()
            .find(|p| p.services.iter().any(|s| s.id == id))
        else {
            return Ok(false);
        };
        if project
            .services
            .iter()
            .any(|s| s.id != id && shared::normalize_name(&s.spec.name) == new_safe)
        {
            anyhow::bail!(
                "já existe um serviço com o nome \"{}\" neste projeto",
                spec.name
            );
        }
        let owner = project.id.clone();
        let svc = project.services.iter_mut().find(|s| s.id == id).unwrap();
        // Serviço que virou Compose numa edição de fonte ainda não tem stack gravada: grava agora,
        // com o nome de agora. Se já tem, não mexe — é o que faz o rename não trocar a stack.
        if matches!(spec.source, ServiceSource::Compose(_)) && svc.compose_project.is_none() {
            svc.compose_project = Some(compose_name);
        }
        svc.spec = spec.clone();
        // O serviço mora no projeto em que está; o spec acompanha (antes a coluna e o JSON podiam
        // divergir).
        svc.spec.project_id = owner;
        Ok(true)
    })?;
    if !found {
        return Ok(None);
    }
    get(db, id).await
}

/// Remove o job dado da fila de pré-deploy check (`pre_deploy_job_ids`, e do
/// `pre_deploy_job_id` legado) de qualquer serviço que o referencie —
/// cascade do `job_delete` (o job pode ter sido apagado em Schedules
/// enquanto ainda referenciado como check de um ou mais serviços; sem essa
/// limpeza o deploy falha em `DeployState::PreDeployCheck` com "job não
/// encontrado", e a UI mostra a fila sem esse item porque o seletor só lista
/// jobs que existem hoje, mascarando o ID órfão que continua salvo no
/// spec). Preserva a ordem dos itens restantes. Retorna quantos serviços
/// foram atualizados.
pub async fn clear_pre_deploy_job(db: &Db, job_id: &str) -> Result<u64> {
    db.infra().update(|st| {
        let mut cleared = 0u64;
        for svc in st.projects.iter_mut().flat_map(|p| p.services.iter_mut()) {
            let legacy = svc.spec.pre_deploy_job_id.as_deref() == Some(job_id);
            let queued = svc.spec.pre_deploy_job_ids.iter().any(|j| j == job_id);
            if !legacy && !queued {
                continue;
            }
            if legacy {
                svc.spec.pre_deploy_job_id = None;
            }
            svc.spec.pre_deploy_job_ids.retain(|j| j != job_id);
            cleared += 1;
        }
        Ok(cleared)
    })
}

pub async fn update_status(
    db: &Db,
    id: &str,
    status: &ServiceStatus,
    container_id: Option<&str>,
) -> Result<()> {
    info!(
        service_id = %id,
        status = %status,
        container_id = ?container_id.map(
            |container_id| format!("...{}", &container_id[..container_id.len().min(10)])
        ),
        "db::services::update_status"
    );
    // Só grava para serviço que existe (igual ao comportamento de antes).
    let exists = db
        .infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
        .any(|s| s.id == id);
    if exists {
        sqlx::query(
            "INSERT INTO service_runtime (service_id, status, live_container_id, desired_state, updated_at)
             VALUES (?, ?, ?, 'Stopped', ?)
             ON CONFLICT(service_id) DO UPDATE SET
                status = excluded.status,
                -- Queued/Deploying sem container: o deploy novo ainda está sendo construído e o container
                -- ANTIGO segue no ar. Apagar o id aqui cegava logs, métricas, watchdog e o parar durante
                -- todo o build, e fazia o reconcile achar que o Live antigo não rodava (vira Stopped).
                live_container_id = CASE
                    WHEN excluded.live_container_id IS NULL
                         AND excluded.status IN ('Queued', 'Deploying')
                    THEN service_runtime.live_container_id
                    ELSE excluded.live_container_id
                END,
                updated_at = excluded.updated_at",
        )
        .bind(id)
        .bind(status.to_string())
        .bind(container_id)
        .bind(Utc::now())
        .execute(db.pool())
        .await?;
    }
    debug!(service_id = %id, status = %status, "db::services::update_status: atualizado");
    Ok(())
}

pub async fn delete(db: &Db, id: &str) -> Result<bool> {
    // Servidor compartilhado removido: os acessos dele não valem mais.
    super::shared_access::delete_for_server(db, id).await?;
    super::managed_database::delete_for_server(db, id).await?;
    let removed = db.infra().update(|st| {
        let mut removed = false;
        for p in &mut st.projects {
            let before = p.services.len();
            p.services.retain(|s| s.id != id);
            removed |= p.services.len() != before;
        }
        Ok(removed)
    })?;
    sqlx::query("DELETE FROM service_runtime WHERE service_id = ?")
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(removed)
}

/// Intenção do usuário para um serviço, **distinta do `status` observado**: `Running` = "deve
/// estar no ar" (gravado por Iniciar/deploy), `Stopped` = "parado de propósito, ou nunca
/// iniciado". É isso que o autostart do boot consulta (Fase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Stopped => "Stopped",
        }
    }
}

/// Grava a intenção. Serviço sem linha de runtime ganha uma; serviço que não existe é ignorado.
pub async fn set_desired_state(db: &Db, id: &str, state: DesiredState) -> Result<()> {
    if get(db, id).await?.is_none() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO service_runtime (service_id, status, live_container_id, desired_state, updated_at)
         VALUES (?, 'Stopped', NULL, ?, ?)
         ON CONFLICT(service_id) DO UPDATE SET desired_state = excluded.desired_state",
    )
    .bind(id)
    .bind(state.as_str())
    .bind(Utc::now())
    .execute(db.pool())
    .await?;
    Ok(())
}

#[allow(dead_code)] // usado pelos testes e pela UI (badge "Parado (manual)"), que ainda não existe
pub async fn desired_state(db: &Db, id: &str) -> Result<DesiredState> {
    let v: Option<String> =
        sqlx::query_scalar("SELECT desired_state FROM service_runtime WHERE service_id = ?")
            .bind(id)
            .fetch_optional(db.pool())
            .await?;
    Ok(match v.as_deref() {
        Some("Running") => DesiredState::Running,
        _ => DesiredState::Stopped,
    })
}

/// Serviços que o usuário quer no ar (`desired_state = Running`), qualquer que seja o `status`
/// observado agora. Entrada do autostart no boot.
pub async fn get_desired_running(db: &Db) -> Result<Vec<Service>> {
    filtered(db, |r| r.2 == "Running").await
}

pub async fn get_running(db: &Db) -> Result<Vec<Service>> {
    filtered(db, |r| r.0 == "Running").await
}

pub async fn count_by_project(db: &Db, project_id: &str) -> Result<i64> {
    Ok(db
        .infra()
        .snapshot()
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .map(|p| p.services.len() as i64)
        .unwrap_or(0))
}

pub async fn get_watchable(db: &Db) -> Result<Vec<Service>> {
    filtered(db, |r| r.0 == "Running" || r.0 == "Degraded").await
}

pub async fn list_all(db: &Db) -> Result<Vec<Service>> {
    Ok(all_services(db)
        .await?
        .into_iter()
        .map(|(s, _)| s)
        .collect())
}

#[cfg(test)]
fn test_compose_spec(name: String, project_id: String) -> ServiceSpec {
    ServiceSpec {
        name,
        project_id,
        source: ServiceSource::Compose(shared::ComposeSource {
            content: String::new(),
            ingress_service: None,
        }),
        port: 80,
        host_port: None,
        tcp_ports: vec![],
        domain: None,
        tls_enabled: false,
        env_vars: vec![],
        env_comments: vec![],
        volumes: vec![],
        healthcheck: shared::Healthcheck::default(),
        replicas: 1,
        resources: shared::ResourceLimits::default(),
        run_command: None,
        run_args: vec![],
        product: None,
        domains: vec![],
        pre_deploy_job_id: None,
        pre_deploy_job_ids: vec![],
        depends_on: vec![],
        registry_auto_deploy: false,
        autostart: true,
        shared: None,
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use shared::ServiceSource;

    fn spec(name: &str, project: &str) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            project_id: project.into(),
            source: ServiceSource::Registry {
                image: "nginx:latest".into(),
            },
            port: 80,
            host_port: None,
            tcp_ports: vec![],
            domain: None,
            tls_enabled: false,
            env_vars: vec![],
            env_comments: vec![],
            volumes: vec![],
            healthcheck: shared::Healthcheck::default(),
            replicas: 1,
            resources: shared::ResourceLimits::default(),
            run_command: None,
            run_args: vec![],
            product: None,
            domains: vec![],
            pre_deploy_job_id: None,
            pre_deploy_job_ids: vec![],
            depends_on: vec![],
            registry_auto_deploy: false,
            autostart: true,
            shared: None,
        }
    }

    async fn mem_db() -> Db {
        let db = Db::memory_for_test().await;
        db.seed_project_for_test("proj_a");
        db.seed_project_for_test("proj_b");
        db
    }

    #[tokio::test]
    async fn rejeita_nome_duplicado_no_mesmo_projeto() {
        let db = mem_db().await;
        create(&db, spec("api", "proj_a")).await.unwrap();
        // Mesmo nome exato → erro.
        assert!(create(&db, spec("api", "proj_a")).await.is_err());
        // Nome que normaliza para o mesmo safe_name ("my-api" == "my_api") → erro.
        create(&db, spec("my-api", "proj_a")).await.unwrap();
        assert!(create(&db, spec("my_api", "proj_a")).await.is_err());
    }

    fn names(list: &[Service]) -> Vec<String> {
        list.iter().map(|s| s.spec.name.clone()).collect()
    }

    #[tokio::test]
    async fn reorder_muda_a_ordem_e_vai_ao_yaml() {
        let db = mem_db().await;
        let a = create(&db, spec("a", "proj_a")).await.unwrap();
        let b = create(&db, spec("b", "proj_a")).await.unwrap();
        let c = create(&db, spec("c", "proj_a")).await.unwrap();
        assert_eq!(names(&list(&db, "proj_a").await.unwrap()), ["a", "b", "c"]);

        reorder(&db, "proj_a", &[c.id.clone(), a.id.clone(), b.id.clone()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(names(&list(&db, "proj_a").await.unwrap()), ["c", "a", "b"]);
        assert_eq!(names(&list_all(&db).await.unwrap()), ["c", "a", "b"]);

        // A ordem mora no project.yml, em `service_order`.
        let yaml = String::from_utf8(
            db.infra()
                .tree()
                .read("projects/proj_a/project.yml")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let bloco = yaml.split("service_order:").nth(1).expect(&yaml);
        let pos = |id: &str| bloco.find(id).unwrap();
        assert!(pos(&c.id) < pos(&a.id) && pos(&a.id) < pos(&b.id), "{yaml}");

        // Um serviço novo entra no fim; apagar um serviço poda o id da ordem.
        let d = create(&db, spec("d", "proj_a")).await.unwrap();
        assert_eq!(
            names(&list(&db, "proj_a").await.unwrap()),
            ["c", "a", "b", "d"]
        );
        delete(&db, &a.id).await.unwrap();
        assert_eq!(names(&list(&db, "proj_a").await.unwrap()), ["c", "b", "d"]);
        let yaml = String::from_utf8(
            db.infra()
                .tree()
                .read("projects/proj_a/project.yml")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            yaml.split("service_order:")
                .nth(1)
                .is_none_or(|b| !b.contains(&a.id)),
            "{yaml}"
        );
        let _ = d;
    }

    #[tokio::test]
    async fn reorder_mantem_no_fim_quem_nao_veio_e_recusa_id_ruim() {
        let db = mem_db().await;
        let a = create(&db, spec("a", "proj_a")).await.unwrap();
        let b = create(&db, spec("b", "proj_a")).await.unwrap();
        let c = create(&db, spec("c", "proj_a")).await.unwrap();
        let outro = create(&db, spec("x", "proj_b")).await.unwrap();

        // Só `c` informado: `a` e `b` seguem depois, na ordem em que estavam.
        reorder(&db, "proj_a", std::slice::from_ref(&c.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(names(&list(&db, "proj_a").await.unwrap()), ["c", "a", "b"]);

        // Id de outro projeto, repetido ou projeto inexistente: erro e nada muda.
        assert!(
            reorder(&db, "proj_a", std::slice::from_ref(&outro.id))
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            reorder(&db, "proj_a", &[a.id.clone(), a.id.clone()])
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            reorder(&db, "nao_existe", std::slice::from_ref(&b.id))
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(names(&list(&db, "proj_a").await.unwrap()), ["c", "a", "b"]);
    }

    #[tokio::test]
    async fn spec_vai_ao_yaml_e_runtime_fica_no_sql() {
        let db = mem_db().await;
        let svc = create(&db, spec("api", "proj_a")).await.unwrap();
        update_status(
            &db,
            &svc.id,
            &ServiceStatus::Running,
            Some("cont_secreto_1"),
        )
        .await
        .unwrap();
        let yaml = String::from_utf8(
            db.infra()
                .tree()
                .read("projects/proj_a/project.yml")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(yaml.contains("nginx:latest"), "o spec deve estar no YAML");
        assert!(
            !yaml.contains("Running") && !yaml.contains("cont_secreto_1"),
            "status e container são runtime e não podem ir ao YAML:\n{yaml}"
        );
        // a tabela antiga `service` não recebe mais nada
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM service")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn criar_em_projeto_inexistente_falha_sem_deixar_rastro() {
        let db = mem_db().await;
        let e = create(&db, spec("api", "nao_existe")).await.unwrap_err();
        assert!(e.to_string().contains("não encontrado"), "{e}");
        assert!(list_all(&db).await.unwrap().is_empty());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM service_runtime")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn webhook_token_acompanha_o_servico_no_yaml_cifrado() {
        use crate::store::webhook_tokens as wh;
        let db = std::sync::Arc::new(mem_db().await);
        let sm = crate::platform::secrets::SecretsManager::new(db.clone()).unwrap();
        let svc = create(&db, spec("api", "proj_a")).await.unwrap();
        assert!(wh::get(&db, &sm, &svc.id).await.unwrap().is_none());
        wh::upsert(&db, &sm, &svc.id, "tok1").await.unwrap();
        wh::upsert(&db, &sm, &svc.id, "tok2-visivel").await.unwrap();
        assert_eq!(
            wh::get(&db, &sm, &svc.id).await.unwrap().as_deref(),
            Some("tok2-visivel")
        );
        // no disco só existe o texto cifrado
        let yaml = String::from_utf8(
            db.infra()
                .tree()
                .read("projects/proj_a/project.yml")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(yaml.contains("ENC[age:") && !yaml.contains("tok2-visivel"));
        assert!(wh::upsert(&db, &sm, "fantasma", "x").await.is_err());
        delete(&db, &svc.id).await.unwrap();
        assert!(wh::get(&db, &sm, &svc.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn status_e_container_vivem_em_service_runtime() {
        let db = mem_db().await;
        let svc = create(&db, spec("api", "proj_a")).await.unwrap();
        assert_eq!(svc.status, ServiceStatus::Stopped);

        update_status(&db, &svc.id, &ServiceStatus::Running, Some("cont_1"))
            .await
            .unwrap();
        let got = get(&db, &svc.id).await.unwrap().unwrap();
        assert_eq!(got.status, ServiceStatus::Running);
        assert_eq!(got.live_container_id.as_deref(), Some("cont_1"));

        // A verdade é a tabela nova (as colunas legadas são só dupla escrita).
        let (st,): (String,) =
            sqlx::query_as("SELECT status FROM service_runtime WHERE service_id = ?")
                .bind(&svc.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(st, "Running");

        // Filtros por runtime enxergam a tabela nova.
        assert_eq!(get_running(&db).await.unwrap().len(), 1);
        assert_eq!(get_watchable(&db).await.unwrap().len(), 1);
        update_status(&db, &svc.id, &ServiceStatus::Stopped, None)
            .await
            .unwrap();
        assert!(get_running(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_status_de_servico_inexistente_nao_cria_linha_de_runtime() {
        let db = mem_db().await;
        update_status(&db, "svc_fantasma", &ServiceStatus::Running, None)
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM service_runtime")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn desired_state_comeca_parado_e_so_o_boot_ve_os_desejados_no_ar() {
        let db = mem_db().await;
        let a = create(&db, spec("a", "proj_a")).await.unwrap();
        let b = create(&db, spec("b", "proj_a")).await.unwrap();
        assert_eq!(
            desired_state(&db, &a.id).await.unwrap(),
            DesiredState::Stopped
        );
        assert!(get_desired_running(&db).await.unwrap().is_empty());

        set_desired_state(&db, &a.id, DesiredState::Running)
            .await
            .unwrap();
        let ids: Vec<String> = get_desired_running(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec![a.id.clone()]);

        // Mudar o status observado não mexe na intenção.
        update_status(&db, &a.id, &ServiceStatus::Stopped, None)
            .await
            .unwrap();
        assert_eq!(
            desired_state(&db, &a.id).await.unwrap(),
            DesiredState::Running
        );

        set_desired_state(&db, &a.id, DesiredState::Stopped)
            .await
            .unwrap();
        assert!(get_desired_running(&db).await.unwrap().is_empty());
        let _ = b;
    }

    #[tokio::test]
    async fn queued_e_deploying_preservam_o_container_ao_vivo_e_os_demais_estados_limpam() {
        let db = mem_db().await;
        let svc = create(&db, spec("api", "proj_a")).await.unwrap();
        let cid = |db: &Db, id: String| {
            let db = db.clone();
            async move { get(&db, &id).await.unwrap().unwrap().live_container_id }
        };
        update_status(&db, &svc.id, &ServiceStatus::Running, Some("c_velho"))
            .await
            .unwrap();
        update_status(&db, &svc.id, &ServiceStatus::Queued, None)
            .await
            .unwrap();
        assert_eq!(cid(&db, svc.id.clone()).await.as_deref(), Some("c_velho"));
        update_status(&db, &svc.id, &ServiceStatus::Deploying, None)
            .await
            .unwrap();
        assert_eq!(cid(&db, svc.id.clone()).await.as_deref(), Some("c_velho"));
        // um id novo vence
        update_status(&db, &svc.id, &ServiceStatus::Running, Some("c_novo"))
            .await
            .unwrap();
        assert_eq!(cid(&db, svc.id.clone()).await.as_deref(), Some("c_novo"));
        // parar limpa
        update_status(&db, &svc.id, &ServiceStatus::Stopped, None)
            .await
            .unwrap();
        assert_eq!(cid(&db, svc.id.clone()).await, None);
    }

    #[tokio::test]
    async fn delete_apaga_tambem_o_runtime() {
        let db = mem_db().await;
        let svc = create(&db, spec("api", "proj_a")).await.unwrap();
        assert!(delete(&db, &svc.id).await.unwrap());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM service_runtime")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn migracao_preenche_runtime_de_banco_antigo_e_preserva_a_intencao() {
        let db = mem_db().await;
        // Simula um banco pré-Fase 2: serviços só com as colunas legadas.
        let up = create(&db, spec("up", "proj_a")).await.unwrap();
        let down = create(&db, spec("down", "proj_a")).await.unwrap();
        // A tabela antiga `service` não é mais escrita; o banco "antigo" é simulado à mão.
        for (svc, status) in [(&up, "Running"), (&down, "Stopped")] {
            sqlx::query(
                "INSERT INTO service (id, name, project_id, spec, status, created_at, updated_at)
                 VALUES (?, ?, 'proj_a', '{}', ?, ?, ?)",
            )
            .bind(&svc.id)
            .bind(&svc.spec.name)
            .bind(status)
            .bind(Utc::now())
            .bind(Utc::now())
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query("DELETE FROM service_runtime")
            .execute(db.pool())
            .await
            .unwrap();

        super::super::connect_existing_for_test(&db).await;

        assert_eq!(
            get(&db, &up.id).await.unwrap().unwrap().status,
            ServiceStatus::Running
        );
        assert_eq!(
            desired_state(&db, &up.id).await.unwrap(),
            DesiredState::Running
        );
        assert_eq!(
            desired_state(&db, &down.id).await.unwrap(),
            DesiredState::Stopped
        );

        // Reexecutar a migração não sobrescreve a intenção já gravada.
        set_desired_state(&db, &up.id, DesiredState::Stopped)
            .await
            .unwrap();
        super::super::connect_existing_for_test(&db).await;
        assert_eq!(
            desired_state(&db, &up.id).await.unwrap(),
            DesiredState::Stopped
        );
    }

    #[tokio::test]
    async fn permite_mesmo_nome_em_projetos_diferentes() {
        let db = mem_db().await;
        create(&db, spec("api", "proj_a")).await.unwrap();
        // Projetos têm redes isoladas → sem colisão.
        assert!(create(&db, spec("api", "proj_b")).await.is_ok());
    }

    #[tokio::test]
    async fn rename_nao_pode_colidir_mas_permite_o_proprio_nome() {
        let db = mem_db().await;
        create(&db, spec("api", "proj_a")).await.unwrap();
        let web = create(&db, spec("web", "proj_a")).await.unwrap();

        // Renomear "web" → "api" (já existe) deve falhar.
        assert!(
            update_spec(&db, &web.id, spec("api", "proj_a"))
                .await
                .is_err()
        );
        // Renomear para um nome livre funciona.
        assert!(
            update_spec(&db, &web.id, spec("web2", "proj_a"))
                .await
                .unwrap()
                .is_some()
        );
        // Regravar o próprio serviço com o mesmo nome (não é colisão) funciona.
        assert!(
            update_spec(&db, &web.id, spec("web2", "proj_a"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn clear_pre_deploy_job_limpa_so_servicos_que_referenciam_o_job_legado() {
        let db = mem_db().await;
        let mut with_job = spec("api", "proj_a");
        with_job.pre_deploy_job_id = Some("job_alvo".into());
        let api = create(&db, with_job).await.unwrap();

        let mut with_other_job = spec("web", "proj_a");
        with_other_job.pre_deploy_job_id = Some("job_outro".into());
        let web = create(&db, with_other_job).await.unwrap();

        let cleared = clear_pre_deploy_job(&db, "job_alvo").await.unwrap();
        assert_eq!(cleared, 1);

        assert_eq!(
            get(&db, &api.id)
                .await
                .unwrap()
                .unwrap()
                .spec
                .pre_deploy_job_id,
            None
        );
        assert_eq!(
            get(&db, &web.id)
                .await
                .unwrap()
                .unwrap()
                .spec
                .pre_deploy_job_id,
            Some("job_outro".into())
        );
    }

    /// Serviço com a fila NOVA (`pre_deploy_job_ids`): o job removido some da
    /// lista, mas os outros ficam — e na MESMA ordem relativa (só o item
    /// removido sai do meio).
    #[tokio::test]
    async fn clear_pre_deploy_job_remove_so_o_item_da_fila_preservando_ordem() {
        let db = mem_db().await;
        let mut with_queue = spec("api", "proj_a");
        with_queue.pre_deploy_job_ids =
            vec!["job_migration".into(), "job_alvo".into(), "job_test".into()];
        let api = create(&db, with_queue).await.unwrap();

        let cleared = clear_pre_deploy_job(&db, "job_alvo").await.unwrap();
        assert_eq!(cleared, 1);

        assert_eq!(
            get(&db, &api.id)
                .await
                .unwrap()
                .unwrap()
                .spec
                .pre_deploy_job_ids,
            vec!["job_migration".to_string(), "job_test".to_string()]
        );
    }

    #[tokio::test]
    async fn clear_pre_deploy_job_no_op_quando_ninguem_referencia() {
        let db = mem_db().await;
        let mut with_queue = spec("api", "proj_a");
        with_queue.pre_deploy_job_ids = vec!["job_migration".into()];
        create(&db, with_queue).await.unwrap();

        let cleared = clear_pre_deploy_job(&db, "job_nunca_referenciado")
            .await
            .unwrap();
        assert_eq!(cleared, 0);
    }
}

/// Chaves de serviço (`services:`) de um compose, para casar com as de uma stack
/// viva. Vazio se o YAML não parseia ou não tem `services`.
fn compose_service_keys(content: &str) -> std::collections::BTreeSet<String> {
    serde_yaml::from_str::<serde_yaml::Value>(content)
        .ok()
        .and_then(|v| v.get("services").and_then(|s| s.as_mapping().cloned()))
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Grava `compose_project` nos serviços Compose que ainda não têm, com o nome
/// **que a stack já usa** — nenhum container ou volume em produção muda de
/// nome. `live` são as stacks que existem hoje no Docker (nome → chaves de
/// serviço dos containers): cobre o serviço renomeado e ainda não redeployado,
/// cuja stack viva (a que tem os volumes) é a do nome antigo.
///
/// Uma stack só é adotada por um serviço **renomeado** se, além do prefixo do
/// nome (8 primeiros chars do ID — igual para tudo criado no mesmo segundo, como
/// num import de manifesto), as chaves de serviço dos containers cabem nas do
/// compose do serviço, e só uma stack serve. Sem isso um serviço qualquer sem
/// stack levaria a de outro. Sem como decidir, vale a fórmula legada com o nome
/// atual — o mesmo que o daemon já calculava. Depois cria o índice `UNIQUE`.
/// Roda no boot, com o Docker no ar.
pub async fn backfill_compose_projects(
    db: &Db,
    live: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Result<()> {
    let services: Vec<Service> = list_all(db).await?;
    // O que decidimos gravar: `(id do serviço, nome da stack)`. Aplicado de uma vez no fim.
    let mut decided: Vec<(String, String)> = vec![];
    let compose: Vec<&Service> = services
        .iter()
        .filter(|s| matches!(s.spec.source, ServiceSource::Compose(_)))
        .collect();

    // Nomes que não podem ser "achados" por outro serviço: os já gravados e a
    // fórmula legada de cada serviço Compose.
    let mut taken: std::collections::HashSet<String> = compose
        .iter()
        .filter_map(|s| s.compose_project.clone())
        .collect();
    let legacy_of = |s: &Service| shared::compose_project_name(&s.id, &s.spec.name);
    let legacy_names: std::collections::HashSet<String> =
        compose.iter().map(|s| legacy_of(s)).collect();

    // Primeiro os serviços cuja stack viva tem exatamente o nome legado (o caso
    // comum), para que essas stacks saiam de `taken`/candidatas dos demais.
    let (exact, rest): (Vec<&&Service>, Vec<&&Service>) = compose
        .iter()
        .filter(|s| s.compose_project.is_none())
        .partition(|s| live.contains_key(&legacy_of(s)));
    for svc in exact {
        let legacy = legacy_of(svc);
        decided.push((svc.id.clone(), legacy.clone()));
        taken.insert(legacy);
    }

    for svc in rest {
        let legacy = legacy_of(svc);
        let ServiceSource::Compose(c) = &svc.spec.source else {
            continue;
        };
        let keys = compose_service_keys(&c.content);
        let id_prefix = {
            let legacy_id = svc.id.strip_prefix("svc_").unwrap_or(&svc.id);
            format!(
                "rp_{}_",
                legacy_id.get(..8).unwrap_or(legacy_id).to_lowercase()
            )
        };
        let candidates: Vec<&String> = live
            .iter()
            .filter(|(p, _)| p.starts_with(&id_prefix))
            .filter(|(p, _)| !taken.contains(*p) && !legacy_names.contains(*p))
            // As chaves de serviço da stack cabem nas do compose deste serviço.
            .filter(|(_, stack_keys)| !stack_keys.is_empty() && stack_keys.is_subset(&keys))
            .map(|(p, _)| p)
            .collect();
        let chosen = if candidates.len() == 1 {
            info!(
                service_id = %svc.id, name = %svc.spec.name, stack = %candidates[0],
                "db::services::backfill: stack viva com nome antigo (serviço renomeado sem redeploy)"
            );
            candidates[0].clone()
        } else {
            if candidates.len() > 1 {
                warn!(
                    service_id = %svc.id, name = %svc.spec.name, ?candidates,
                    "db::services::backfill: mais de uma stack candidata; usando o nome legado"
                );
            }
            legacy
        };
        decided.push((svc.id.clone(), chosen.clone()));
        taken.insert(chosen);
    }

    if !decided.is_empty() {
        db.infra().update(|st| {
            for svc in st.projects.iter_mut().flat_map(|p| p.services.iter_mut()) {
                if svc.compose_project.is_none()
                    && let Some((_, name)) = decided.iter().find(|(id, _)| *id == svc.id)
                {
                    svc.compose_project = Some(name.clone());
                }
            }
            Ok(())
        })?;
    }

    // Antes havia um índice UNIQUE no SQL; no YAML a unicidade é garantida na criação
    // (`new_compose_project`). Dado antigo repetido só gera aviso, como o índice fazia.
    let mut seen = std::collections::HashSet::new();
    for s in db
        .infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
    {
        if let Some(c) = &s.compose_project
            && !seen.insert(c.clone())
        {
            warn!(stack = %c, "db::services::backfill: duas stacks Compose com o mesmo nome; separar exige decidir de quem são os volumes");
        }
    }

    Ok(())
}

#[cfg(test)]
mod nome_gravado_tests {
    use super::*;
    use crate::store::projects;

    async fn temp_db() -> Db {
        let dir = std::env::temp_dir().join(format!("rustploy-test-{}", Ulid::new()));
        crate::store::connect(&dir).await.unwrap()
    }

    /// Simula um banco antigo: apaga o nome da stack gravado (de um serviço, ou de todos).
    fn esquece_stack(db: &Db, id: Option<&str>) {
        db.infra()
            .update(|st| {
                for s in st.projects.iter_mut().flat_map(|p| p.services.iter_mut()) {
                    if id.is_none_or(|i| i == s.id) {
                        s.compose_project = None;
                    }
                }
                Ok(())
            })
            .unwrap();
    }

    /// Dá um id escolhido a um serviço (para simular um import no mesmo segundo).
    fn troca_id(db: &Db, de: &str, para: &str) {
        db.infra()
            .update(|st| {
                for s in st.projects.iter_mut().flat_map(|p| p.services.iter_mut()) {
                    if s.id == de {
                        s.id = para.to_string();
                    }
                }
                Ok(())
            })
            .unwrap();
    }

    /// Stacks vivas do Docker: `(nome da stack, chaves de serviço dos containers)`.
    fn live(
        v: &[(&str, &[&str])],
    ) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        v.iter()
            .map(|(p, keys)| (p.to_string(), keys.iter().map(|k| k.to_string()).collect()))
            .collect()
    }

    #[tokio::test]
    async fn projeto_novo_grava_rede_com_id_inteiro_e_dois_juntos_nao_colidem() {
        let db = temp_db().await;
        let a = projects::create(&db, "a".into(), None).await.unwrap();
        let b = projects::create(&db, "b".into(), None).await.unwrap();
        let na = projects::network_name(&db, &a.id).await.unwrap();
        let nb = projects::network_name(&db, &b.id).await.unwrap();
        assert_eq!(na, shared::new_project_network_name(&a.id));
        assert!(na.len() > "rp_net_".len() + 20, "ID inteiro: {na}");
        assert_ne!(na, nb);
    }

    #[tokio::test]
    async fn migracao_preenche_rede_com_o_nome_legado() {
        let db = temp_db().await;
        sqlx::query(
            "INSERT INTO project (id, name, env_vars, created_at) VALUES ('prj_01ABCDEFGHJK', 'velho', '[]', ?)",
        )
        .bind(Utc::now())
        .execute(db.pool())
        .await
        .unwrap();
        // Roda a migração de novo, como num boot seguinte.
        crate::store::connect_existing_for_test(&db).await;
        assert_eq!(
            projects::network_name(&db, "prj_01ABCDEFGHJK")
                .await
                .unwrap(),
            "rp_net_01ABCDEF"
        );
    }

    #[tokio::test]
    async fn rename_de_servico_compose_nao_muda_a_stack() {
        let db = temp_db().await;
        let p = projects::create(&db, "p".into(), None).await.unwrap();
        let spec = test_compose_spec("postgres".into(), p.id.clone());
        let svc = create(&db, spec).await.unwrap();
        let stack = svc.compose_project.clone().expect("Compose grava a stack");
        assert_eq!(stack, shared::new_compose_project_name(&svc.id, "postgres"));

        let mut spec = svc.spec.clone();
        spec.name = "banco".into();
        let renomeado = update_spec(&db, &svc.id, spec).await.unwrap().unwrap();
        assert_eq!(renomeado.spec.name, "banco");
        assert_eq!(renomeado.compose_project_name(), stack);
    }

    #[tokio::test]
    async fn servico_que_nao_e_compose_nao_grava_stack() {
        let db = temp_db().await;
        let p = projects::create(&db, "p".into(), None).await.unwrap();
        let spec = test_compose_spec("x".into(), p.id.clone());
        let mut spec = spec;
        spec.source = ServiceSource::Registry {
            image: "nginx".into(),
        };
        let svc = create(&db, spec).await.unwrap();
        assert!(svc.compose_project.is_none());
    }

    #[tokio::test]
    async fn backfill_usa_a_stack_viva_de_servico_renomeado_sem_redeploy() {
        let db = temp_db().await;
        let p = projects::create(&db, "p".into(), None).await.unwrap();
        let mut spec = test_compose_spec("postgres".into(), p.id.clone());
        spec.source = ServiceSource::Compose(shared::ComposeSource {
            content: "services:\n  app:\n    image: x\n".into(),
            ingress_service: None,
        });
        let svc = create(&db, spec).await.unwrap();
        // Simula banco antigo: stack não gravada; serviço renomeado; a stack
        // viva no Docker é a do nome antigo.
        esquece_stack(&db, Some(&svc.id));
        let mut spec = svc.spec.clone();
        spec.name = "banco".into();
        update_spec(&db, &svc.id, spec).await.unwrap();
        esquece_stack(&db, Some(&svc.id));

        let antiga = shared::compose_project_name(&svc.id, "postgres");
        backfill_compose_projects(&db, &live(&[("outra_coisa", &["x"]), (&antiga, &["app"])]))
            .await
            .unwrap();
        let depois = get(&db, &svc.id).await.unwrap().unwrap();
        assert_eq!(depois.compose_project.as_deref(), Some(antiga.as_str()));
    }

    #[tokio::test]
    async fn backfill_sem_stack_no_docker_usa_a_formula_legada_com_o_nome_atual() {
        let db = temp_db().await;
        let p = projects::create(&db, "p".into(), None).await.unwrap();
        let svc = create(&db, test_compose_spec("db".into(), p.id.clone()))
            .await
            .unwrap();
        esquece_stack(&db, Some(&svc.id));
        backfill_compose_projects(&db, &live(&[])).await.unwrap();
        let depois = get(&db, &svc.id).await.unwrap().unwrap();
        assert_eq!(
            depois.compose_project,
            Some(shared::compose_project_name(&svc.id, "db"))
        );
    }

    #[tokio::test]
    async fn backfill_nao_rouba_a_stack_de_outro_servico() {
        let db = temp_db().await;
        let p = projects::create(&db, "p".into(), None).await.unwrap();
        let a = create(&db, test_compose_spec("a".into(), p.id.clone()))
            .await
            .unwrap();
        let b = create(&db, test_compose_spec("b".into(), p.id.clone()))
            .await
            .unwrap();
        esquece_stack(&db, None);
        // Só a stack de `a` existe no Docker; `b` não pode adotá-la, mesmo se
        // os dois tivessem os mesmos 8 primeiros chars do ID.
        let stack_a = shared::compose_project_name(&a.id, "a");
        backfill_compose_projects(&db, &live(&[(&stack_a, &["app"])]))
            .await
            .unwrap();
        let a2 = get(&db, &a.id).await.unwrap().unwrap();
        let b2 = get(&db, &b.id).await.unwrap().unwrap();
        assert_ne!(a2.compose_project, b2.compose_project);
        assert_eq!(
            b2.compose_project,
            Some(shared::compose_project_name(&b.id, "b"))
        );
    }

    /// Regressão achada no teste com a infra de produção: um import de manifesto
    /// cria todos os serviços no mesmo segundo, então todos têm o mesmo prefixo
    /// de ID. O serviço `db`, que nunca subiu, adotava a stack viva do `broker`
    /// (renomeado para `mq` sem redeploy) por ser o único candidato pelo prefixo.
    #[tokio::test]
    async fn backfill_de_import_no_mesmo_segundo_liga_cada_stack_ao_seu_servico() {
        let db = temp_db().await;
        let p = projects::create(&db, "p".into(), None).await.unwrap();
        let mk = |name: &str, key: &str| {
            let mut spec = test_compose_spec(name.into(), p.id.clone());
            spec.source = ServiceSource::Compose(shared::ComposeSource {
                content: format!("services:\n  {key}:\n    image: x\n"),
                ingress_service: None,
            });
            spec
        };
        let db_svc = create(&db, mk("db", "rp_db")).await.unwrap();
        let broker = create(&db, mk("broker", "rp_broker")).await.unwrap();
        // Mesmo prefixo de 8 chars, como num import.
        for (svc, sufixo) in [
            (&db_svc, "AAAAAAAAAAAAAAAAAAAAAAAA"),
            (&broker, "BBBBBBBBBBBBBBBBBBBBBBBB"),
        ] {
            troca_id(&db, &svc.id, &format!("svc_01M3Q7T1{sufixo}"));
        }
        let broker_id = "svc_01M3Q7T1BBBBBBBBBBBBBBBBBBBBBBBB";
        let db_id = "svc_01M3Q7T1AAAAAAAAAAAAAAAAAAAAAAAA";
        // `broker` foi renomeado para `mq` e a stack viva segue com o nome antigo.
        let mut spec = get(&db, broker_id).await.unwrap().unwrap().spec;
        spec.name = "mq".into();
        update_spec(&db, broker_id, spec).await.unwrap();
        esquece_stack(&db, None);

        backfill_compose_projects(&db, &live(&[("rp_01m3q7t1_broker", &["rp_broker"])]))
            .await
            .unwrap();

        let mq = get(&db, broker_id).await.unwrap().unwrap();
        let db2 = get(&db, db_id).await.unwrap().unwrap();
        assert_eq!(mq.compose_project.as_deref(), Some("rp_01m3q7t1_broker"));
        assert_eq!(db2.compose_project.as_deref(), Some("rp_01m3q7t1_db"));
    }
}
