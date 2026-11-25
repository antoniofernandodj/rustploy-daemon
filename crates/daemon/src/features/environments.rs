//! **Ambientes** (homologação, preview…): criar a partir de um projeto, comparar com a origem e
//! promover a release no ar de um lado para o outro. Ver
//! `docs/arquivo/plano-releases-ambientes-rotas-volumes.md` §2.
//!
//! Um ambiente é um projeto comum com `ProjectInfra.environment`: ganha rede, nomes de container e
//! variáveis próprios sem mexer em nada do que já existe. A cópia é uma transação só no estado de
//! infra (YAML), então ou o ambiente nasce inteiro ou nada muda.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use ulid::Ulid;

use crate::domain::{
    EnvironmentComparison, EnvironmentOf, EnvironmentServicePair, EnvironmentSummary, Project,
    ReleaseSide, ServiceSource, diff_releases, environment_project_name, environment_relevant,
    is_valid_environment_name,
};
use crate::store::Db;
use crate::store::infra::model::{JobInfra, ProjectInfra, SecretInfra, ServiceInfra};

fn new_secret_copy(secret: &SecretInfra) -> SecretInfra {
    SecretInfra {
        id: format!("sec_{}", Ulid::new()),
        key: secret.key.clone(),
        // O texto cifrado vale como está: é o mesmo servidor, a mesma chave.
        value: secret.value.clone(),
    }
}

/// O que a cópia deixou de fora, para a pessoa saber (vai para o log e a resposta da tela).
#[derive(Debug, Default)]
pub struct CopyNotes {
    /// Pares (serviço de origem → serviço do ambiente) cujos arquivos de zip precisam ser copiados.
    pub archives: Vec<(String, String, String)>,
}

/// Monta o projeto-ambiente a partir do de origem (pura, sem disco nem banco: testável).
///
/// - serviços, jobs, variáveis e secrets copiados com ids novos; `depends_on`, vínculos,
///   pré-deploy e serviço gatilho dos jobs reapontados para as cópias;
/// - **não** copia: domínios e portas do host (são únicos no servidor), push-to-deploy do registry
///   (cada push redeployaria os dois), papel de servidor compartilhado, política de backup, bancos
///   gerenciados e acessos a bancos compartilhados (o ambiente não deve alcançar os dados da origem);
///   vínculos e dependências para fora do projeto (são esses bancos) saem também;
/// - jobs **agendados** nascem desligados (não rodar duas vezes o que roda na origem).
pub fn build_environment(
    origin: &ProjectInfra,
    environment_name: &str,
    taken_compose_project: &dyn Fn(&str, &str) -> String,
) -> (ProjectInfra, CopyNotes) {
    let project_id = format!("prj_{}", Ulid::new());
    let mut notes = CopyNotes::default();
    let service_map: HashMap<String, String> = origin
        .services
        .iter()
        .map(|service| (service.id.clone(), format!("svc_{}", Ulid::new())))
        .collect();
    let job_map: HashMap<String, String> = origin
        .jobs
        .iter()
        .map(|job| (job.id.clone(), format!("job_{}", Ulid::new())))
        .collect();

    let services = origin
        .services
        .iter()
        .map(|service| {
            let new_id = service_map[&service.id].clone();
            let mut spec = service.spec.clone();
            spec.project_id = project_id.clone();
            spec.domain = None;
            spec.domains = Vec::new();
            spec.tls_enabled = false;
            spec.host_port = None;
            spec.tcp_ports = Vec::new();
            spec.registry_auto_deploy = false;
            spec.shared = None;
            spec.depends_on = spec
                .depends_on
                .iter()
                .filter_map(|dependency| service_map.get(dependency).cloned())
                .collect();
            spec.links = spec
                .links
                .iter()
                .filter_map(|link| {
                    service_map.get(&link.service_id).map(|mapped| {
                        let mut link = link.clone();
                        link.service_id = mapped.clone();
                        link
                    })
                })
                .collect();
            spec.pre_deploy_job_ids = spec
                .pre_deploy_job_ids
                .iter()
                .filter_map(|job_id| job_map.get(job_id).cloned())
                .collect();
            spec.pre_deploy_job_id = spec
                .pre_deploy_job_id
                .as_ref()
                .and_then(|job_id| job_map.get(job_id).cloned());
            if let ServiceSource::Archive(archive) = &spec.source
                && !archive.archive_id.is_empty()
            {
                notes.archives.push((
                    service.id.clone(),
                    new_id.clone(),
                    archive.archive_id.clone(),
                ));
            }
            let compose_project = match &spec.source {
                ServiceSource::Compose(_) => Some(taken_compose_project(&new_id, &spec.name)),
                _ => None,
            };
            ServiceInfra {
                id: new_id,
                spec,
                compose_project,
                created_at: Utc::now(),
                webhook_token: None,
                secrets: service.secrets.iter().map(new_secret_copy).collect(),
                backup: None,
            }
        })
        .collect();

    let jobs = origin
        .jobs
        .iter()
        .map(|job| JobInfra {
            id: job_map[&job.id].clone(),
            trigger_service_id: job
                .trigger_service_id
                .as_ref()
                .and_then(|service_id| service_map.get(service_id).cloned()),
            secrets: job.secrets.iter().map(new_secret_copy).collect(),
            enabled: job.enabled && job.recurrence.is_none(),
            created_at: Utc::now(),
            ..job.clone()
        })
        .collect();

    let environment = ProjectInfra {
        id: project_id.clone(),
        name: environment_project_name(&origin.name, environment_name.trim()),
        description: origin.description.clone(),
        env_vars: origin.env_vars.clone(),
        env_comments: origin.env_comments.clone(),
        network_name: Some(crate::domain::new_project_network_name(&project_id)),
        created_at: Utc::now(),
        services,
        jobs,
        secrets: origin.secrets.iter().map(new_secret_copy).collect(),
        managed_databases: Vec::new(),
        shared_access: Vec::new(),
        service_order: origin
            .service_order
            .iter()
            .filter_map(|service_id| service_map.get(service_id).cloned())
            .collect(),
        environment: Some(EnvironmentOf {
            origin_project_id: origin.id.clone(),
            name: environment_name.trim().to_string(),
            service_origins: service_map
                .iter()
                .map(|(origin_id, environment_id)| (environment_id.clone(), origin_id.clone()))
                .collect::<BTreeMap<_, _>>(),
        }),
        // Rotas não vêm: os domínios são únicos no servidor (como os do serviço).
        routes: Vec::new(),
    };
    (environment, notes)
}

/// A pasta do zip extraído de um serviço (a mesma de `service_archive_upload::archive_extract_dir`
/// e do `DeployExecutor::archive_dir`; repetida aqui porque `features` não depende de `api`).
fn archive_dir(db_path: &Path, service_id: &str, archive_id: &str) -> std::path::PathBuf {
    db_path
        .join("uploads")
        .join("services")
        .join(service_id)
        .join(archive_id)
}

fn copy_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

/// Cria o ambiente `name` a partir do projeto `project_id`. Não faz deploy: os serviços nascem
/// parados, como num projeto novo.
pub async fn create(db: &Db, db_path: &Path, project_id: &str, name: &str) -> Result<Project> {
    if !is_valid_environment_name(name) {
        bail!("nome de ambiente inválido: use de 1 a 40 caracteres, sem parênteses");
    }
    let state = db.infra().snapshot();
    let origin = state
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .ok_or_else(|| anyhow!("projeto {project_id} não encontrado"))?;
    if origin.environment.is_some() {
        bail!("este projeto já é um ambiente; crie o ambiente a partir do projeto de origem");
    }
    let (environment, notes) = build_environment(origin, name, &|service_id, service_name| {
        crate::store::services::new_compose_project(&state, service_id, service_name)
    });
    let environment_id = environment.id.clone();

    // Os zips ficam numa pasta por serviço: a cópia precisa dos seus, senão o primeiro deploy
    // não acha o que construir. Antes da transação: se falhar, nada foi gravado.
    for (origin_service_id, environment_service_id, archive_id) in &notes.archives {
        let source = archive_dir(db_path, origin_service_id, archive_id);
        if source.is_dir() {
            let target = archive_dir(db_path, environment_service_id, archive_id);
            copy_dir(&source, &target)
                .map_err(|e| anyhow!("não consegui copiar o zip de {origin_service_id}: {e}"))?;
        }
    }

    db.infra().update(move |st| {
        st.projects.push(environment);
        Ok(())
    })?;
    tracing::info!(origin = %project_id, environment = %environment_id, name = %name, "ambiente criado");
    crate::store::projects::get(db, &environment_id)
        .await?
        .ok_or_else(|| anyhow!("ambiente criado mas não encontrado: {environment_id}"))
}

/// Os ambientes de um projeto de origem.
pub fn list(db: &Db, origin_project_id: &str) -> Vec<EnvironmentSummary> {
    db.infra()
        .snapshot()
        .projects
        .iter()
        .filter_map(|project| {
            let environment = project.environment.as_ref()?;
            (environment.origin_project_id == origin_project_id).then(|| EnvironmentSummary {
                project_id: project.id.clone(),
                project_name: project.name.clone(),
                name: environment.name.clone(),
            })
        })
        .collect()
}

/// Compara um ambiente com a origem, serviço a serviço.
pub async fn compare(db: &Db, environment_project_id: &str) -> Result<EnvironmentComparison> {
    let state = db.infra().snapshot();
    let environment_project = state
        .projects
        .iter()
        .find(|project| project.id == environment_project_id)
        .ok_or_else(|| anyhow!("projeto {environment_project_id} não encontrado"))?;
    let link = environment_project
        .environment
        .as_ref()
        .ok_or_else(|| anyhow!("este projeto não é um ambiente"))?;
    let origin_project = state
        .projects
        .iter()
        .find(|project| project.id == link.origin_project_id)
        .ok_or_else(|| anyhow!("o projeto de origem não existe mais"))?;

    let mut services = Vec::new();
    let mut paired_origins = Vec::new();
    let mut only_in_environment = Vec::new();
    for environment_service in &environment_project.services {
        let Some(origin_service) = link
            .service_origins
            .get(&environment_service.id)
            .and_then(|origin_id| origin_project.services.iter().find(|s| &s.id == origin_id))
        else {
            only_in_environment.push(environment_service.spec.name.clone());
            continue;
        };
        paired_origins.push(origin_service.id.clone());
        let origin_release =
            crate::features::deploy::release::current_release(db, &origin_service.id).await?;
        let environment_release =
            crate::features::deploy::release::current_release(db, &environment_service.id).await?;
        let is_compose = matches!(origin_service.spec.source, ServiceSource::Compose(_))
            || matches!(environment_service.spec.source, ServiceSource::Compose(_));
        let origin_image = origin_release.as_ref().and_then(|r| r.image_ref.clone());
        let environment_image = environment_release
            .as_ref()
            .and_then(|r| r.image_ref.clone());
        // No ar desde antes das releases (v0.1.91): sem release, mas com um deploy Live e o
        // container rodando. Ao levar, a imagem do container vira a release (ver
        // `ensure_baseline_release`).
        let origin_running_without_release = !is_compose
            && origin_image.is_none()
            && running_without_release(db, &origin_service.id).await;
        let environment_running_without_release = !is_compose
            && environment_image.is_none()
            && running_without_release(db, &environment_service.id).await;
        let origin_has = origin_image.is_some() || origin_running_without_release;
        let environment_has = environment_image.is_some() || environment_running_without_release;
        let different = origin_image.is_none() || origin_image != environment_image;
        let can_promote_to_origin = !is_compose && environment_has && different;
        let can_bring_from_origin = !is_compose && origin_has && different;
        let note = if is_compose {
            Some("compose")
        } else if !origin_has && !environment_has {
            Some("no_release_anywhere")
        } else if (can_bring_from_origin && origin_running_without_release)
            || (can_promote_to_origin && environment_running_without_release)
        {
            Some("running_without_release")
        } else if !origin_has {
            Some("no_release_in_origin")
        } else if !environment_has {
            Some("no_release_here")
        } else {
            None
        };
        // Os ids de `depends_on`, vínculos e pré-deploy são outros na cópia (apontam para os
        // serviços e jobs dela): compara pelos nomes, senão a cópia fiel aparece como diferente.
        let origin_named = spec_with_names(&origin_service.spec, &state);
        let environment_named = spec_with_names(&environment_service.spec, &state);
        let changes = environment_relevant(diff_releases(
            &ReleaseSide {
                image_ref: None,
                spec: &origin_named,
                project_env: &[],
            },
            &ReleaseSide {
                image_ref: None,
                spec: &environment_named,
                project_env: &[],
            },
        ));
        services.push(EnvironmentServicePair {
            service_name: environment_service.spec.name.clone(),
            origin_service_id: origin_service.id.clone(),
            environment_service_id: environment_service.id.clone(),
            origin_release_number: origin_release.as_ref().map(|r| r.number),
            environment_release_number: environment_release.as_ref().map(|r| r.number),
            origin_release_id: origin_release.as_ref().map(|r| r.id.clone()),
            environment_release_id: environment_release.as_ref().map(|r| r.id.clone()),
            same_image: origin_image.is_some() && origin_image == environment_image,
            // Com a mesma imagem dos dois lados não há o que levar.
            can_promote_to_origin,
            can_bring_from_origin,
            note: note.map(str::to_string),
            changes,
        });
    }
    let only_in_origin = origin_project
        .services
        .iter()
        .filter(|service| !paired_origins.contains(&service.id))
        .map(|service| service.spec.name.clone())
        .collect();
    let project_env_changes = diff_releases(
        &ReleaseSide {
            image_ref: None,
            spec: &origin_project
                .services
                .first()
                .map(|s| s.spec.clone())
                .unwrap_or_else(placeholder_spec),
            project_env: &origin_project.env_vars,
        },
        &ReleaseSide {
            image_ref: None,
            spec: &origin_project
                .services
                .first()
                .map(|s| s.spec.clone())
                .unwrap_or_else(placeholder_spec),
            project_env: &environment_project.env_vars,
        },
    );
    Ok(EnvironmentComparison {
        origin_project_id: origin_project.id.clone(),
        origin_project_name: origin_project.name.clone(),
        environment_project_id: environment_project.id.clone(),
        environment_name: link.name.clone(),
        services,
        project_env_changes,
        only_in_origin,
        only_in_environment,
    })
}

/// O spec com os ids de `depends_on`, vínculos e pré-deploy trocados pelos nomes (do serviço ou do
/// job, procurados em todos os projetos). Só para comparar: a cópia de um ambiente aponta para os
/// serviços dela, com ids próprios, e o que importa é "depende do db", não qual id.
pub fn spec_with_names(
    spec: &crate::domain::ServiceSpec,
    state: &crate::store::infra::model::InfraState,
) -> crate::domain::ServiceSpec {
    let service_name = |id: &str| {
        state
            .projects
            .iter()
            .flat_map(|project| project.services.iter())
            .find(|service| service.id == id)
            .map(|service| service.spec.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let job_name = |id: &str| {
        state
            .projects
            .iter()
            .flat_map(|project| project.jobs.iter())
            .find(|job| job.id == id)
            .map(|job| job.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let mut named = spec.clone();
    named.depends_on = spec.depends_on.iter().map(|id| service_name(id)).collect();
    for link in &mut named.links {
        link.service_id = service_name(&link.service_id);
    }
    named.pre_deploy_job_ids = spec
        .pre_deploy_job_ids
        .iter()
        .map(|id| job_name(id))
        .collect();
    named.pre_deploy_job_id = spec.pre_deploy_job_id.as_deref().map(job_name);
    named
}

/// O serviço está no ar sem release: há um deploy `Live` (de antes das releases existirem) e um
/// container vivo.
async fn running_without_release(db: &Db, service_id: &str) -> bool {
    let Ok(Some(service)) = crate::store::services::get(db, service_id).await else {
        return false;
    };
    if service.live_container_id.is_none() {
        return false;
    }
    live_deployment_id(db, service_id).await.is_some()
}

async fn live_deployment_id(db: &Db, service_id: &str) -> Option<String> {
    crate::store::deployments::list_for_service(db, service_id, 50)
        .await
        .ok()?
        .into_iter()
        .find(|deployment| deployment.state == crate::domain::DeployState::Live)
        .map(|deployment| deployment.id)
}

/// A release no ar do serviço; se ele está no ar desde antes das releases (deploy Live sem
/// release), registra a imagem do container que está rodando como a release daquele deploy e a
/// devolve. Só para fonte que não é Compose. A configuração gravada é a de agora (a do deploy antigo
/// não foi guardada). `None` = nada no ar.
pub async fn ensure_baseline_release(
    db: &Db,
    docker: &crate::engine::DockerClient,
    service_id: &str,
) -> Result<Option<crate::domain::Release>> {
    if let Some(release) = crate::features::deploy::release::current_release(db, service_id).await?
    {
        return Ok(Some(release));
    }
    let Some(service) = crate::store::services::get(db, service_id).await? else {
        return Ok(None);
    };
    if matches!(service.spec.source, ServiceSource::Compose(_)) {
        return Ok(None);
    }
    let (Some(container_id), Some(deployment_id)) = (
        service.live_container_id.clone(),
        live_deployment_id(db, service_id).await,
    ) else {
        return Ok(None);
    };
    let container = crate::engine::containers::inspect(&docker.inner, &container_id)
        .await
        .map_err(|e| anyhow!("não consegui inspecionar o container no ar: {e}"))?;
    let image_id = container
        .image
        .clone()
        .ok_or_else(|| anyhow!("o container no ar não diz de qual imagem veio"))?;
    let image = container
        .config
        .as_ref()
        .and_then(|config| config.image.clone())
        .unwrap_or_else(|| image_id.clone());
    let repo_digests = match docker.inner.inspect_image(&image_id).await {
        Ok(inspected) => inspected.repo_digests.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let image_ref = crate::domain::pick_image_ref(
        matches!(service.spec.source, ServiceSource::Registry { .. }),
        &image,
        Some(&image_id),
        &repo_digests,
    );
    let project_env = match crate::store::projects::get(db, &service.spec.project_id).await? {
        Some(project) => project.env_vars,
        None => Vec::new(),
    };
    let release = crate::store::releases::create(
        db,
        crate::store::releases::NewRelease {
            service_id,
            deployment_id: &deployment_id,
            image: &image,
            image_ref: image_ref.as_deref(),
            spec: &service.spec,
            project_env: &project_env,
            restored_from_number: None,
            promoted_from: None,
        },
    )
    .await?;
    tracing::info!(service_id = %service_id, release = release.number, image = %image,
        "release registrada a partir do container no ar");
    Ok(Some(release))
}

/// Um spec qualquer, só para comparar as variáveis do projeto pelo mesmo `diff_releases` quando a
/// origem não tem serviço (os dois lados usam o mesmo spec, então ele não gera diferença).
fn placeholder_spec() -> crate::domain::ServiceSpec {
    crate::features::wizard::app_spec(String::new(), String::new())
}

/// Os dois serviços são irmãos (um é a cópia do outro num ambiente)?
pub fn are_siblings(db: &Db, first_service_id: &str, second_service_id: &str) -> bool {
    db.infra().snapshot().projects.iter().any(|project| {
        project.environment.as_ref().is_some_and(|link| {
            link.service_origins
                .get(first_service_id)
                .map(String::as_str)
                == Some(second_service_id)
                || link
                    .service_origins
                    .get(second_service_id)
                    .map(String::as_str)
                    == Some(first_service_id)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DomainRoute, ServiceLink};

    fn service(id: &str, name: &str) -> ServiceInfra {
        ServiceInfra {
            id: id.into(),
            spec: crate::features::wizard::app_spec(name.into(), "prj_origem".into()),
            compose_project: None,
            created_at: Utc::now(),
            webhook_token: Some("ENC[age:token]".into()),
            secrets: vec![SecretInfra {
                id: "sec_1".into(),
                key: "API_KEY".into(),
                value: "ENC[age:x]".into(),
            }],
            backup: None,
        }
    }

    #[test]
    fn copia_reaponta_e_deixa_de_fora_o_que_e_unico() {
        let mut api = service("svc_api", "api");
        api.spec.domains = vec![DomainRoute {
            domain: "app.com".into(),
            port: None,
            tls: true,
        }];
        api.spec.host_port = Some(20001);
        api.spec.registry_auto_deploy = true;
        api.spec.depends_on = vec!["svc_db".into(), "svc_de_outro_projeto".into()];
        api.spec.links = vec![
            ServiceLink {
                service_id: "svc_db".into(),
                ..Default::default()
            },
            ServiceLink {
                service_id: "svc_banco_compartilhado".into(),
                ..Default::default()
            },
        ];
        api.spec.pre_deploy_job_ids = vec!["job_migra".into()];
        let db = service("svc_db", "db");
        let origin = ProjectInfra {
            id: "prj_origem".into(),
            name: "loja".into(),
            description: None,
            env_vars: vec![],
            env_comments: vec![],
            network_name: Some("rp_net_x".into()),
            created_at: Utc::now(),
            services: vec![api, db],
            jobs: vec![
                JobInfra {
                    id: "job_migra".into(),
                    trigger_service_id: Some("svc_api".into()),
                    name: "migra".into(),
                    compose: String::new(),
                    git_source: None,
                    main_service: "m".into(),
                    env_vars: vec![],
                    env_comments: vec![],
                    secrets: vec![],
                    enabled: true,
                    recurrence: None,
                    created_at: Utc::now(),
                },
                JobInfra {
                    id: "job_noturno".into(),
                    trigger_service_id: None,
                    name: "noturno".into(),
                    compose: String::new(),
                    git_source: None,
                    main_service: "m".into(),
                    env_vars: vec![],
                    env_comments: vec![],
                    secrets: vec![],
                    enabled: true,
                    recurrence: Some(crate::domain::Recurrence::IntervalHours(1)),
                    created_at: Utc::now(),
                },
            ],
            secrets: vec![],
            managed_databases: vec![],
            shared_access: vec![],
            service_order: vec!["svc_db".into(), "svc_api".into()],
            environment: None,
            routes: vec![],
        };
        let (copy, _) = build_environment(&origin, " homologação ", &|_, name| name.to_string());
        assert_eq!(copy.name, "loja (homologação)");
        let link = copy.environment.as_ref().unwrap();
        assert_eq!(link.name, "homologação");
        assert_eq!(link.origin_project_id, "prj_origem");
        let new_api = copy.services.iter().find(|s| s.spec.name == "api").unwrap();
        let new_db = copy.services.iter().find(|s| s.spec.name == "db").unwrap();
        assert_ne!(new_api.id, "svc_api");
        assert_eq!(link.service_origins[&new_api.id], "svc_api");
        assert_eq!(new_api.spec.project_id, copy.id);
        assert!(new_api.spec.domains.is_empty() && new_api.spec.host_port.is_none());
        assert!(!new_api.spec.registry_auto_deploy);
        assert_eq!(new_api.spec.depends_on, vec![new_db.id.clone()]);
        assert_eq!(new_api.spec.links.len(), 1);
        assert_eq!(new_api.spec.links[0].service_id, new_db.id);
        assert!(new_api.webhook_token.is_none());
        assert_eq!(new_api.secrets[0].value, "ENC[age:x]");
        assert_ne!(new_api.secrets[0].id, "sec_1");
        let migra = copy.jobs.iter().find(|j| j.name == "migra").unwrap();
        assert_eq!(
            migra.trigger_service_id.as_deref(),
            Some(new_api.id.as_str())
        );
        assert!(migra.enabled);
        assert_eq!(new_api.spec.pre_deploy_job_ids, vec![migra.id.clone()]);
        assert!(
            !copy
                .jobs
                .iter()
                .find(|j| j.name == "noturno")
                .unwrap()
                .enabled
        );
        assert_eq!(
            copy.service_order,
            vec![new_db.id.clone(), new_api.id.clone()]
        );
        assert_ne!(copy.network_name, origin.network_name);

        // A cópia fiel não é "diferente" só porque os ids de depends_on, vínculos e pré-deploy
        // são os dela: a comparação troca ids por nomes.
        let state = crate::store::infra::model::InfraState {
            projects: vec![origin.clone(), copy.clone()],
            ..Default::default()
        };
        let origin_api = origin.services.iter().find(|s| s.id == "svc_api").unwrap();
        let named_origin = spec_with_names(&origin_api.spec, &state);
        let mut named_copy = spec_with_names(&new_api.spec, &state);
        named_copy.project_id = named_origin.project_id.clone();
        // O que a cópia tirou de propósito (o serviço de outro projeto) continua aparecendo.
        assert_eq!(named_origin.depends_on, vec!["db", "svc_de_outro_projeto"]);
        assert_eq!(named_copy.depends_on, vec!["db"]);
        assert_eq!(named_copy.links[0].service_id, "db");
        assert_eq!(named_copy.pre_deploy_job_ids, vec!["migra"]);
        assert_eq!(
            named_origin.pre_deploy_job_ids,
            named_copy.pre_deploy_job_ids
        );
        let changes = environment_relevant(diff_releases(
            &ReleaseSide {
                image_ref: None,
                spec: &named_origin,
                project_env: &[],
            },
            &ReleaseSide {
                image_ref: None,
                spec: &named_copy,
                project_env: &[],
            },
        ));
        let fields: Vec<&str> = changes.iter().map(|c| c.field.as_str()).collect();
        assert!(
            !fields.iter().any(|f| f.starts_with("pre_deploy")),
            "pré-deploy igual por nome: {fields:?}"
        );
    }
}
