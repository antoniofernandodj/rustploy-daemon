//! Quem a limpeza aquece em seguida: a lista de serviços (com build) e jobs, com a
//! escolha de cada um. Fonte única da verdade (`Command::WarmAfterCleanupTargets`):
//! as telas só exibem e mandam a escolha, nunca calculam o padrão.
//!
//! A escolha explícita mora em `DockerCleanupConfig::warm_after_cleanup_choices`.
//! Sem ela vale o padrão: todo serviço com build aquece (o comportamento de antes da
//! lista) e um job aquece só se for pré-deploy de algum serviço, porque é ele que
//! segura o deploy enquanto constrói a própria imagem.

use super::MaintenanceContext;
use crate::domain::{
    DockerCleanupConfig, Job, Project, Service, WarmAfterCleanupTarget, WarmAfterCleanupTargets,
    WarmTargetKind,
};
use crate::store;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub enum ChoiceError {
    NotFound,
    Database(anyhow::Error),
}

/// A lista, a partir do que está gravado agora.
pub async fn load(state: &MaintenanceContext) -> anyhow::Result<WarmAfterCleanupTargets> {
    let config = super::run::load_config(state).await?;
    let services = store::services::list_all(&state.db).await?;
    let jobs = store::job::list_all(&state.db).await?;
    let projects = store::projects::list(&state.db).await?;
    Ok(WarmAfterCleanupTargets {
        cleanup_enabled: config.enabled && config.any_resource_enabled(),
        warm_after_cleanup: config.warm_after_cleanup,
        targets: build(&config, &services, &jobs, &projects),
    })
}

/// Grava a escolha de `id`. Escolher o padrão apaga a escolha, para o item voltar a
/// seguir o padrão (um job que vira pré-deploy depois passa a aquecer sozinho).
pub async fn set_choice(
    state: &MaintenanceContext,
    id: &str,
    warm: bool,
) -> Result<WarmAfterCleanupTargets, ChoiceError> {
    let current = load(state).await.map_err(ChoiceError::Database)?;
    let Some(target) = current.targets.iter().find(|target| target.id == id) else {
        return Err(ChoiceError::NotFound);
    };
    let mut config = super::run::load_config(state)
        .await
        .map_err(ChoiceError::Database)?;
    if warm == target.default_warm {
        config.warm_after_cleanup_choices.remove(id);
    } else {
        config
            .warm_after_cleanup_choices
            .insert(id.to_string(), warm);
    }
    super::run::save_config(state, &config)
        .await
        .map_err(ChoiceError::Database)?;
    load(state).await.map_err(ChoiceError::Database)
}

/// A decisão, sem banco nem Docker.
pub fn build(
    config: &DockerCleanupConfig,
    services: &[Service],
    jobs: &[Job],
    projects: &[Project],
) -> Vec<WarmAfterCleanupTarget> {
    let project_names: HashMap<&str, &str> = projects
        .iter()
        .map(|project| (project.id.as_str(), project.name.as_str()))
        .collect();
    let project_name = |project_id: &str| {
        project_names
            .get(project_id)
            .copied()
            .unwrap_or(project_id)
            .to_string()
    };
    let pre_deploy_jobs: HashSet<String> = services
        .iter()
        .flat_map(|svc| svc.spec.pre_deploy_checks())
        .collect();
    let choice = |id: &str, default_warm: bool| {
        config
            .warm_after_cleanup_choices
            .get(id)
            .copied()
            .unwrap_or(default_warm)
    };

    let mut targets: Vec<WarmAfterCleanupTarget> = services
        .iter()
        .filter(|svc| super::service_warm::is_warmable(svc))
        .map(|svc| WarmAfterCleanupTarget {
            id: svc.id.clone(),
            kind: WarmTargetKind::Service,
            project_id: svc.spec.project_id.clone(),
            project_name: project_name(&svc.spec.project_id),
            name: svc.spec.name.clone(),
            warm: choice(&svc.id, true),
            default_warm: true,
            pre_deploy: false,
        })
        .collect();
    targets.extend(jobs.iter().map(|job| {
        let pre_deploy = pre_deploy_jobs.contains(&job.id);
        WarmAfterCleanupTarget {
            id: job.id.clone(),
            kind: WarmTargetKind::Job,
            project_id: job.project_id.clone(),
            project_name: project_name(&job.project_id),
            name: job.name.clone(),
            warm: choice(&job.id, pre_deploy),
            default_warm: pre_deploy,
            pre_deploy,
        }
    }));
    targets.sort_by(|a, b| {
        (a.project_name.to_lowercase(), a.kind, a.name.to_lowercase()).cmp(&(
            b.project_name.to_lowercase(),
            b.kind,
            b.name.to_lowercase(),
        ))
    });
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        GitSource, Healthcheck, ResourceLimits, ServiceSource, ServiceSpec, ServiceStatus,
    };
    use chrono::Utc;

    fn service(id: &str, project_id: &str, source: ServiceSource, jobs: &[&str]) -> Service {
        Service {
            id: id.into(),
            spec: ServiceSpec {
                description: None,
                name: format!("nome-{id}"),
                project_id: project_id.into(),
                source,
                port: 80,
                host_port: None,
                tcp_ports: vec![],
                domain: None,
                tls_enabled: false,
                env_vars: vec![],
                env_comments: vec![],
                volumes: vec![],
                healthcheck: Healthcheck::default(),
                replicas: 1,
                resources: ResourceLimits::default(),
                run_command: None,
                run_args: vec![],
                product: None,
                domains: vec![],
                pre_deploy_job_id: None,
                pre_deploy_job_ids: jobs.iter().map(|job| job.to_string()).collect(),
                shared: None,
                depends_on: vec![],
                registry_auto_deploy: false,
                autostart: true,
                links: Vec::new(),
            },
            compose_project: None,
            status: ServiceStatus::Stopped,
            live_container_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn git() -> ServiceSource {
        ServiceSource::Git(GitSource {
            url: "https://example.com/r.git".into(),
            branch: "main".into(),
            ..GitSource::default()
        })
    }

    fn job(id: &str, project_id: &str, name: &str) -> Job {
        let mut job: Job = serde_json::from_value(serde_json::json!({
            "id": id, "project_id": project_id, "trigger_service_id": null, "name": name,
            "compose": "", "main_service": "main", "recurrence": null, "enabled": true,
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .expect("job de teste");
        job.name = name.into();
        job
    }

    fn project(id: &str, name: &str) -> Project {
        serde_json::from_value(
            serde_json::json!({"id": id, "name": name, "description": null,
            "created_at": "2026-01-01T00:00:00Z"}),
        )
        .expect("projeto de teste")
    }

    fn fixture() -> (Vec<Service>, Vec<Job>, Vec<Project>) {
        (
            vec![
                service("svc_api", "prj_flow", git(), &["job_migrate"]),
                service(
                    "svc_db",
                    "prj_flow",
                    ServiceSource::Registry {
                        image: "mysql:8".into(),
                    },
                    &[],
                ),
            ],
            vec![
                job("job_migrate", "prj_flow", "migrate"),
                job("job_backup", "prj_flow", "backup"),
            ],
            vec![project("prj_flow", "Flow")],
        )
    }

    fn ids_and_warm(targets: &[WarmAfterCleanupTarget]) -> Vec<(&str, bool)> {
        targets
            .iter()
            .map(|target| (target.id.as_str(), target.warm))
            .collect()
    }

    #[test]
    fn by_default_services_with_a_build_and_pre_deploy_jobs_warm() {
        let (services, jobs, projects) = fixture();
        let targets = build(&DockerCleanupConfig::default(), &services, &jobs, &projects);
        // Sem build (imagem pronta) não entra; serviço antes de job; jobs por nome.
        assert_eq!(
            ids_and_warm(&targets),
            vec![
                ("svc_api", true),
                ("job_backup", false),
                ("job_migrate", true)
            ]
        );
        assert!(targets.iter().all(|target| target.project_name == "Flow"));
    }

    #[test]
    fn an_explicit_choice_wins_over_the_default() {
        let (services, jobs, projects) = fixture();
        let mut config = DockerCleanupConfig::default();
        config
            .warm_after_cleanup_choices
            .insert("svc_api".into(), false);
        config
            .warm_after_cleanup_choices
            .insert("job_backup".into(), true);
        let targets = build(&config, &services, &jobs, &projects);
        assert_eq!(
            ids_and_warm(&targets),
            vec![
                ("svc_api", false),
                ("job_backup", true),
                ("job_migrate", true)
            ]
        );
    }

    #[test]
    fn a_config_saved_before_the_choices_still_loads() {
        let mut json = serde_json::to_value(DockerCleanupConfig::default()).unwrap();
        assert!(json.get("warm_after_cleanup_choices").is_none());
        json.as_object_mut()
            .unwrap()
            .remove("warm_after_cleanup_choices");
        let loaded: DockerCleanupConfig = serde_json::from_value(json).unwrap();
        assert!(loaded.warm_after_cleanup_choices.is_empty());
    }
}
