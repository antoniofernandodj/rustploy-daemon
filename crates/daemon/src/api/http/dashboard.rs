//! O painel "Em execução" (`dashboard` no snapshot): só os projetos que têm
//! serviço rodando e, dentro de cada um, só os serviços rodando — já
//! classificados por tipo (app Git/zip/imagem, banco, broker, compose) e com o
//! que o cartão mostra (branch, domínios, webhook, pré-deploy checks, selo
//! de aquecimento). A GUI e a webui só desenham este valor: a regra "quem é
//! banco, quem é broker, o que conta como rodando" mora aqui, uma vez.

use crate::bootstrap::state::AppState;
use crate::features::maintenance::service_warm;
use crate::store;
use serde::Serialize;
use shared::{Job, Project, Service, ServiceSource, ServiceStatus, ServiceWarmStatus};
use std::collections::HashMap;

const DATABASE_KINDS: &[&str] = &["postgres", "mongodb", "mariadb", "mysql", "redis"];
const BROKER_KINDS: &[&str] = &["kafka", "rabbitmq", "nats"];

#[derive(Debug, Serialize, PartialEq)]
pub struct DashboardProject {
    pub project_id: String,
    pub project_name: String,
    pub services: Vec<DashboardService>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DashboardService {
    pub service_id: String,
    pub name: String,
    /// `Running` ou `Degraded` — os dois contam como "em execução".
    pub status: String,
    /// `git` | `archive` | `registry` | `compose` | `database` | `broker`.
    pub kind: String,
    pub kind_label: String,
    /// Só `git`: a branch implantada e o repositório.
    pub branch: Option<String>,
    pub repo_url: Option<String>,
    /// Só `registry`: imagem:tag.
    pub image: Option<String>,
    /// Só `database`/`broker`: o motor (postgres, kafka…).
    pub engine: Option<String>,
    pub port: u16,
    pub domains: Vec<String>,
    /// URL completa do webhook de deploy (com o token); `None` sem token ou em
    /// serviço Compose, que não tem webhook.
    pub webhook: Option<String>,
    pub pre_deploy_checks: Vec<DashboardCheck>,
    /// `None` em serviço sem build.
    pub warm: Option<ServiceWarmStatus>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DashboardCheck {
    pub job_id: String,
    /// Nome do job; o id quando o job foi apagado.
    pub name: String,
}

/// Tipo do serviço e o rótulo curto do cartão.
fn classify(svc: &Service) -> (&'static str, String) {
    if let Some(engine) = svc.spec.db_kind.as_deref() {
        let engine = engine.to_lowercase();
        if BROKER_KINDS.contains(&engine.as_str()) {
            return ("broker", format!("Broker · {engine}"));
        }
        let _ = DATABASE_KINDS; // qualquer outro `db_kind` também é banco
        return ("database", format!("Banco · {engine}"));
    }
    match &svc.spec.source {
        ServiceSource::Git(_) => ("git", "App · Git".to_string()),
        ServiceSource::Archive(_) => ("archive", "App · zip".to_string()),
        ServiceSource::Registry { .. } => ("registry", "App · imagem".to_string()),
        ServiceSource::Compose(_) => ("compose", "Compose".to_string()),
    }
}

fn is_running(status: &ServiceStatus) -> bool {
    matches!(status, ServiceStatus::Running | ServiceStatus::Degraded)
}

/// A montagem, sem tocar em banco nem em Docker. Projetos sem serviço rodando
/// ficam de fora; a ordem é a de `projects` e, dentro dele, a de `services`.
pub fn build(
    projects: &[Project],
    services: &[Service],
    jobs: &[Job],
    webhook_urls: &HashMap<String, String>,
    warm: &HashMap<String, ServiceWarmStatus>,
) -> Vec<DashboardProject> {
    let job_names: HashMap<&str, &str> = jobs
        .iter()
        .map(|job| (job.id.as_str(), job.name.as_str()))
        .collect();

    projects
        .iter()
        .filter_map(|project| {
            let running: Vec<DashboardService> = services
                .iter()
                .filter(|svc| svc.spec.project_id == project.id && is_running(&svc.status))
                .map(|svc| {
                    let (kind, kind_label) = classify(svc);
                    let (branch, repo_url) = match &svc.spec.source {
                        ServiceSource::Git(git) => {
                            (Some(git.branch.clone()), Some(git.url.clone()))
                        }
                        _ => (None, None),
                    };
                    let image = match &svc.spec.source {
                        ServiceSource::Registry { image } => Some(image.clone()),
                        _ => None,
                    };
                    let engine = matches!(kind, "database" | "broker")
                        .then(|| svc.spec.db_kind.clone())
                        .flatten();
                    DashboardService {
                        service_id: svc.id.clone(),
                        name: svc.spec.name.clone(),
                        status: if svc.status == ServiceStatus::Running {
                            "Running".to_string()
                        } else {
                            "Degraded".to_string()
                        },
                        kind: kind.to_string(),
                        kind_label,
                        branch,
                        repo_url,
                        image,
                        engine,
                        port: svc.spec.port,
                        domains: svc
                            .spec
                            .domain_routes()
                            .into_iter()
                            .map(|route| route.domain)
                            .collect(),
                        webhook: webhook_urls.get(&svc.id).cloned(),
                        pre_deploy_checks: svc
                            .spec
                            .pre_deploy_checks()
                            .into_iter()
                            .map(|job_id| DashboardCheck {
                                name: job_names
                                    .get(job_id.as_str())
                                    .map_or_else(|| job_id.clone(), |name| (*name).to_string()),
                                job_id,
                            })
                            .collect(),
                        warm: warm.get(&svc.id).cloned(),
                    }
                })
                .collect();
            (!running.is_empty()).then(|| DashboardProject {
                project_id: project.id.clone(),
                project_name: project.name.clone(),
                services: running,
            })
        })
        .collect()
}

/// O painel completo, lendo o que precisa do banco. Falha de leitura vira
/// lista vazia: o snapshot não pode cair por causa do painel.
pub async fn load(state: &AppState) -> Vec<DashboardProject> {
    let Ok(projects) = store::projects::list(&state.db).await else {
        return Vec::new();
    };
    let Ok(services) = store::services::list_all(&state.db).await else {
        return Vec::new();
    };
    let jobs = store::job::list_all(&state.db).await.unwrap_or_default();

    let mut webhook_urls = HashMap::new();
    for svc in services.iter().filter(|svc| is_running(&svc.status)) {
        if matches!(svc.spec.source, ServiceSource::Compose(_)) {
            continue;
        }
        if let Ok(Some(token)) = store::webhook_tokens::get(&state.db, &svc.id).await {
            webhook_urls.insert(
                svc.id.clone(),
                crate::api::handlers::services::get_webhook_url::build_url(state, &svc.id, &token),
            );
        }
    }
    let warm: HashMap<String, ServiceWarmStatus> =
        service_warm::status_all(&state.maintenance_ctx())
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|status| (status.service_id.clone(), status))
            .collect();

    build(&projects, &services, &jobs, &webhook_urls, &warm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use shared::{GitSource, ServiceSpec};

    fn project(id: &str) -> Project {
        Project {
            id: id.into(),
            name: format!("proj-{id}"),
            description: None,
            env_vars: vec![],
            env_comments: vec![],
            created_at: Utc::now(),
        }
    }

    fn service(
        id: &str,
        project_id: &str,
        status: ServiceStatus,
        source: ServiceSource,
    ) -> Service {
        Service {
            id: id.into(),
            spec: ServiceSpec {
                name: format!("svc-{id}"),
                project_id: project_id.into(),
                source,
                port: 8080,
                host_port: None,
                domain: Some("app.example.com".into()),
                tls_enabled: true,
                env_vars: vec![],
                env_comments: vec![],
                volumes: vec![],
                healthcheck: shared::Healthcheck::default(),
                replicas: 1,
                resources: shared::ResourceLimits::default(),
                run_command: None,
                run_args: vec![],
                db_kind: None,
                domains: vec![],
                pre_deploy_job_id: None,
                pre_deploy_job_ids: vec!["job_a".into(), "job_gone".into()],
                shared: None,
            },
            compose_project: None,
            status,
            live_container_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn git(branch: &str) -> ServiceSource {
        ServiceSource::Git(GitSource {
            url: "https://example.com/r.git".into(),
            branch: branch.into(),
            ..GitSource::default()
        })
    }

    fn registry() -> ServiceSource {
        ServiceSource::Registry {
            image: "postgres:18".into(),
        }
    }

    fn job(id: &str, name: &str) -> Job {
        Job {
            id: id.into(),
            project_id: "p1".into(),
            trigger_service_id: None,
            name: name.into(),
            compose: String::new(),
            git_source: None,
            main_service: "main".into(),
            env_vars: vec![],
            env_comments: vec![],
            enabled: true,
            recurrence: None,
            last_run_at: None,
            next_run_at: None,
            created_at: Utc::now(),
        }
    }

    fn dashboard(projects: &[Project], services: &[Service]) -> Vec<DashboardProject> {
        build(
            projects,
            services,
            &[job("job_a", "migrate")],
            &HashMap::new(),
            &HashMap::new(),
        )
    }

    #[test]
    fn projects_without_running_services_are_left_out() {
        let projects = [project("p1"), project("p2"), project("p3")];
        let services = [
            service("s1", "p1", ServiceStatus::Running, git("main")),
            service("s2", "p2", ServiceStatus::Stopped, git("main")),
        ];
        let result = dashboard(&projects, &services);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].project_id, "p1");
    }

    #[test]
    fn stopped_service_of_a_running_project_is_left_out() {
        let projects = [project("p1")];
        let services = [
            service("s1", "p1", ServiceStatus::Running, git("main")),
            service("s2", "p1", ServiceStatus::Stopped, git("main")),
            service("s3", "p1", ServiceStatus::Error("x".into()), git("main")),
        ];
        let result = dashboard(&projects, &services);
        let names: Vec<_> = result[0]
            .services
            .iter()
            .map(|s| s.service_id.as_str())
            .collect();
        assert_eq!(names, ["s1"]);
    }

    #[test]
    fn degraded_service_counts_as_running() {
        let projects = [project("p1")];
        let services = [service("s1", "p1", ServiceStatus::Degraded, git("main"))];
        assert_eq!(
            dashboard(&projects, &services)[0].services[0].status,
            "Degraded"
        );
    }

    #[test]
    fn git_service_carries_its_branch_and_repository() {
        let projects = [project("p1")];
        let services = [service("s1", "p1", ServiceStatus::Running, git("release"))];
        let svc = &dashboard(&projects, &services)[0].services[0];
        assert_eq!(svc.kind, "git");
        assert_eq!(svc.branch.as_deref(), Some("release"));
        assert_eq!(svc.repo_url.as_deref(), Some("https://example.com/r.git"));
        assert_eq!(svc.domains, ["app.example.com"]);
    }

    #[test]
    fn db_kind_decides_database_or_broker_over_the_source() {
        let projects = [project("p1")];
        let mut database = service("s1", "p1", ServiceStatus::Running, registry());
        database.spec.db_kind = Some("postgres".into());
        let mut broker = service("s2", "p1", ServiceStatus::Running, registry());
        broker.spec.db_kind = Some("kafka".into());
        let plain = service("s3", "p1", ServiceStatus::Running, registry());

        let result = dashboard(&projects, &[database, broker, plain]);
        let services = &result[0].services;
        assert_eq!(services[0].kind, "database");
        assert_eq!(services[0].engine.as_deref(), Some("postgres"));
        assert_eq!(services[1].kind, "broker");
        assert_eq!(services[1].kind_label, "Broker · kafka");
        assert_eq!(services[2].kind, "registry");
        assert_eq!(services[2].image.as_deref(), Some("postgres:18"));
    }

    #[test]
    fn pre_deploy_checks_resolve_job_names_and_keep_the_order() {
        let projects = [project("p1")];
        let services = [service("s1", "p1", ServiceStatus::Running, git("main"))];
        let checks = &dashboard(&projects, &services)[0].services[0].pre_deploy_checks;
        assert_eq!(checks[0].name, "migrate");
        // Job apagado: mostra o id em vez de sumir com o check.
        assert_eq!(checks[1].name, "job_gone");
    }

    #[test]
    fn webhook_comes_from_the_token_map() {
        let projects = [project("p1")];
        let services = [service("s1", "p1", ServiceStatus::Running, git("main"))];
        let mut webhooks = HashMap::new();
        webhooks.insert("s1".to_string(), "https://x/webhook/s1/tok".to_string());
        let result = build(&projects, &services, &[], &webhooks, &HashMap::new());
        assert_eq!(
            result[0].services[0].webhook.as_deref(),
            Some("https://x/webhook/s1/tok")
        );
    }
}
