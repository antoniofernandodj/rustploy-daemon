//! Aquecimento de serviços: deixar no host o cache de build da imagem de um
//! serviço com fonte Git ou zip, para o próximo deploy não baixar a imagem base
//! nem reinstalar dependências do zero. É o que a limpeza automática faz
//! logo depois de apagar o cache (`DockerCleanupConfig::warm_after_cleanup`):
//! sem isso, o primeiro deploy depois da limpeza paga o build inteiro.
//!
//! Mesma ideia do aquecimento de jobs (`jobs::warm`): "quente" é um registro
//! (`WarmRecord`, em `daemon_settings`) conferido contra o estado real do
//! Docker — a imagem do registro precisa existir e o cache de build não pode ter
//! sido limpo depois. O registro nasce num aquecimento e também de um deploy
//! bem-sucedido, que constrói a mesma coisa.

use super::MaintenanceContext;
use super::prune::{self as docker_prune, BuildCachePrune};
use crate::engine::images;
use crate::features::deploy::git;
use crate::features::jobs::warm::cache_pruned_since;
use crate::store::{self, Db, daemon_settings};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared::{Event, Service, ServiceSource, ServiceWarmStatus};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

/// Teto de um aquecimento: ele segura a vaga única da fila de deploy.
const WARM_TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WarmRecord {
    /// Impressão digital da fonte do serviço quando foi aquecido.
    pub fingerprint: String,
    pub warmed_at: DateTime<Utc>,
    /// Tag da imagem construída; some do host se uma limpeza a apagar.
    pub image: String,
}

/// O que identifica "o mesmo build" para o cache: repositório e branch (ou só o
/// zip), Dockerfile e contexto. Um commit ou zip novo NÃO invalida: a imagem
/// base e as camadas de dependências continuam valendo, só as camadas do código
/// novo são refeitas no próximo deploy.
fn fingerprint(source: &ServiceSource) -> Option<String> {
    let mut hasher = Sha256::new();
    let parts: Vec<&str> = match source {
        ServiceSource::Git(git) => vec![
            "git",
            &git.url,
            &git.branch,
            &git.dockerfile_path,
            &git.build_context,
        ],
        ServiceSource::Archive(archive) => {
            vec!["archive", &archive.dockerfile_path, &archive.build_context]
        }
        _ => return None,
    };
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    Some(hex::encode(hasher.finalize()))
}

/// Só serviços com Dockerfile para construir têm o que aquecer.
pub fn is_warmable(svc: &Service) -> bool {
    fingerprint(&svc.spec.source).is_some()
}

fn warm_tag(svc: &Service) -> String {
    format!("rp_{}:warm", svc.spec.safe_name())
}

fn record_key(service_id: &str) -> String {
    format!("{}{service_id}", daemon_settings::KEY_SERVICE_WARM_PREFIX)
}

async fn load(db: &Db, service_id: &str) -> Option<WarmRecord> {
    let raw = daemon_settings::get(db, &record_key(service_id))
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

/// Marca o serviço como aquecido agora, com a imagem recém-construída `image`.
/// Chamado pelo aquecimento e por um deploy bem-sucedido.
pub async fn record(db: &Db, svc: &Service, image: &str) {
    let Some(fingerprint) = fingerprint(&svc.spec.source) else {
        return;
    };
    let record = WarmRecord {
        fingerprint,
        warmed_at: Utc::now(),
        image: image.to_string(),
    };
    let Ok(json) = serde_json::to_string(&record) else {
        return;
    };
    if let Err(e) = daemon_settings::set(db, &record_key(&svc.id), &json).await {
        warn!(service_id = %svc.id, error = %e, "service_warm: falha ao registrar o aquecimento");
    }
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Warm(DateTime<Utc>),
    Cold {
        reason: String,
        warmed_at: Option<DateTime<Utc>>,
    },
}

/// A decisão, sem tocar em Docker nem no banco: `image_present` diz se a imagem
/// do registro ainda existe e `prune` é a última limpeza do cache de build.
fn judge(
    source: &ServiceSource,
    record: Option<&WarmRecord>,
    image_present: bool,
    prune: Option<&BuildCachePrune>,
) -> Verdict {
    let Some(record) = record else {
        return Verdict::Cold {
            reason: "nunca aquecido".to_string(),
            warmed_at: None,
        };
    };
    let cold = |reason: String| Verdict::Cold {
        reason,
        warmed_at: Some(record.warmed_at),
    };
    if fingerprint(source).as_deref() != Some(record.fingerprint.as_str()) {
        return cold("a fonte do serviço mudou desde o último aquecimento".to_string());
    }
    if !image_present {
        return cold(format!("a imagem {} não está mais no host", record.image));
    }
    if let Some(prune) = prune
        && cache_pruned_since(record.warmed_at, prune)
    {
        return cold(format!(
            "o cache de build foi limpo em {}",
            prune.at.format("%d/%m %H:%M UTC")
        ));
    }
    Verdict::Warm(record.warmed_at)
}

/// Fonte única da verdade: se o serviço está quente agora, e por que não, se
/// frio. `None` para serviço sem build (imagem pronta, compose).
pub async fn status(state: &MaintenanceContext, svc: &Service) -> Option<ServiceWarmStatus> {
    if !is_warmable(svc) {
        return None;
    }
    let warming = state
        .warming_services
        .lock()
        .map(|set| set.contains(&svc.id))
        .unwrap_or(false);
    let record = load(&state.db, &svc.id).await;
    let image_present = match &record {
        Some(record) => state
            .docker
            .inner
            .inspect_image(&record.image)
            .await
            .is_ok(),
        None => false,
    };
    let prune = docker_prune::last_build_cache_prune(&state.db).await;

    Some(
        match judge(
            &svc.spec.source,
            record.as_ref(),
            image_present,
            prune.as_ref(),
        ) {
            Verdict::Warm(warmed_at) => ServiceWarmStatus {
                service_id: svc.id.clone(),
                warm: true,
                warming,
                reason: None,
                warmed_at: Some(warmed_at),
            },
            Verdict::Cold { reason, warmed_at } => ServiceWarmStatus {
                service_id: svc.id.clone(),
                warm: false,
                warming,
                reason: Some(reason),
                warmed_at,
            },
        },
    )
}

/// O status de todos os serviços que têm build.
pub async fn status_all(state: &MaintenanceContext) -> Result<Vec<ServiceWarmStatus>> {
    let mut out = Vec::new();
    for svc in store::services::list_all(&state.db).await? {
        if let Some(status) = status(state, &svc).await {
            out.push(status);
        }
    }
    Ok(out)
}

/// Marca `service_id` como aquecendo e avisa os clientes. `false` se já estava.
fn begin(state: &MaintenanceContext, service_id: &str) -> bool {
    let newly_warming = state
        .warming_services
        .lock()
        .map(|mut set| set.insert(service_id.to_string()))
        .unwrap_or(false);
    if newly_warming {
        state.bus.publish(Event::ServiceWarmChanged {
            service_id: service_id.to_string(),
            warming: true,
            success: None,
            error: None,
        });
    }
    newly_warming
}

/// Pede o aquecimento de `svc`. Devolve `Ok(true)` se já estava quente (nada é
/// disparado) e `Ok(false)` se foi para a fila: a vaga única é disputada em
/// segundo plano e o fim chega em `Event::ServiceWarmChanged`. `force`
/// reconstrói sem cache (`--no-cache --pull`).
pub async fn request(
    state: &MaintenanceContext,
    svc: Service,
    force: bool,
) -> Result<bool, String> {
    let Some(current) = status(state, &svc).await else {
        return Err("este serviço não tem build para aquecer".to_string());
    };
    if !force && current.warm {
        return Ok(true);
    }
    if !begin(state, &svc.id) {
        return Err("este serviço já está sendo aquecido".to_string());
    }
    let state = state.clone();
    tokio::spawn(async move { run_warm(&state, &svc, force).await });
    Ok(false)
}

/// Depois de uma limpeza: aquece, um de cada vez, os serviços que ficaram frios.
/// Um de cada vez e cada um pegando a vaga só pelo tempo do próprio build — um
/// deploy que chegue no meio espera no máximo um build, não a fila inteira.
pub async fn warm_cold_services(state: &MaintenanceContext) {
    let services = match store::services::list_all(&state.db).await {
        Ok(services) => services,
        Err(e) => {
            warn!(error = %e, "service_warm: falha ao listar serviços");
            return;
        }
    };
    let mut cold = Vec::new();
    for svc in services {
        if let Some(status) = status(state, &svc).await
            && !status.warm
            && begin(state, &svc.id)
        {
            cold.push(svc);
        }
    }
    if cold.is_empty() {
        return;
    }
    info!(count = cold.len(), "service_warm: aquecendo após a limpeza");
    let state = state.clone();
    tokio::spawn(async move {
        for svc in cold {
            run_warm(&state, &svc, false).await;
        }
    });
}

/// Pega a vaga, constrói e avisa o fim. O serviço já foi marcado em `begin`.
async fn run_warm(state: &MaintenanceContext, svc: &Service, force: bool) {
    let permit = state.deploy_queue.acquire_job_slot().await;
    let outcome = tokio::time::timeout(WARM_TIMEOUT, warm_once(state, svc, force)).await;
    drop(permit);
    let result = outcome.unwrap_or_else(|_| {
        Err(anyhow!(
            "o aquecimento passou de {}s e foi interrompido",
            WARM_TIMEOUT.as_secs()
        ))
    });

    if let Ok(mut set) = state.warming_services.lock() {
        set.remove(&svc.id);
    }
    if let Err(e) = &result {
        warn!(service_id = %svc.id, error = %e, "service_warm: falha ao aquecer");
    }
    state.bus.publish(Event::ServiceWarmChanged {
        service_id: svc.id.clone(),
        warming: false,
        success: Some(result.is_ok()),
        error: result.err().map(|e| e.to_string()),
    });
}

/// Aquece com a vaga já em mãos. Reconfere o status primeiro: um deploy pode ter
/// aquecido o serviço enquanto o pedido esperava a vez.
async fn warm_once(state: &MaintenanceContext, svc: &Service, force: bool) -> Result<()> {
    if !force && status(state, svc).await.is_some_and(|s| s.warm) {
        return Ok(());
    }
    info!(service_id = %svc.id, force, "service_warm: aquecendo");

    let build_dir = state
        .db_path
        .join("builds")
        .join(format!("warm-{}", svc.id.to_lowercase()));
    let result = build_in_dir(state, svc, &build_dir, force).await;
    if build_dir.exists()
        && let Err(e) = tokio::fs::remove_dir_all(&build_dir).await
    {
        warn!(service_id = %svc.id, error = %e, "service_warm: falha ao limpar build_dir (best-effort)");
    }

    let tag = result?;
    record(&state.db, svc, &tag).await;
    Ok(())
}

/// Obtém o código da fonte do serviço e constrói a imagem de aquecimento.
/// Devolve a tag construída.
async fn build_in_dir(
    state: &MaintenanceContext,
    svc: &Service,
    build_dir: &std::path::Path,
    force: bool,
) -> Result<String> {
    let (context, dockerfile_path): (PathBuf, String) = match &svc.spec.source {
        ServiceSource::Git(source) => {
            let (token, username) = git::resolve_clone_credentials(
                &state.db,
                &state.secrets,
                source.provider_id.as_deref(),
                source.credentials.as_deref(),
                source.username.as_deref(),
                &svc.spec.project_id,
            )
            .await;
            git::clone(
                git::CloneOptions {
                    url: &source.url,
                    branch: &source.branch,
                    token: token.as_deref(),
                    username: username.as_deref(),
                    dir: build_dir,
                },
                |_progress| {},
            )
            .await?;
            (
                build_dir.join(&source.build_context),
                source.dockerfile_path.clone(),
            )
        }
        // O build só lê o contexto: dispensa copiar o zip extraído.
        ServiceSource::Archive(source) => (
            state
                .db_path
                .join("uploads")
                .join("services")
                .join(&svc.id)
                .join(&source.archive_id)
                .join(&source.build_context),
            source.dockerfile_path.clone(),
        ),
        _ => return Err(anyhow!("este serviço não tem build para aquecer")),
    };
    if !context.join(&dockerfile_path).is_file() {
        return Err(anyhow!(
            "Dockerfile não encontrado: {dockerfile_path} (build context: {})",
            context.display()
        ));
    }

    let tag = warm_tag(svc);
    images::build_with_output(
        &state.docker.inner,
        &context,
        &dockerfile_path,
        &tag,
        force,
        |_line| async {},
    )
    .await?;
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{ArchiveSource, GitSource};

    fn git_source(branch: &str) -> ServiceSource {
        ServiceSource::Git(GitSource {
            url: "https://example.com/r.git".into(),
            branch: branch.into(),
            ..GitSource::default()
        })
    }

    fn record_for(source: &ServiceSource, warmed_at: DateTime<Utc>) -> WarmRecord {
        WarmRecord {
            fingerprint: fingerprint(source).unwrap(),
            warmed_at,
            image: "rp_api:warm".into(),
        }
    }

    fn prune(at: DateTime<Utc>, older_than_hours: u32) -> BuildCachePrune {
        BuildCachePrune {
            at,
            older_than_hours,
        }
    }

    #[test]
    fn sources_without_a_build_have_nothing_to_warm() {
        let registry = ServiceSource::Registry {
            image: "nginx".into(),
        };
        assert_eq!(fingerprint(&registry), None);
    }

    #[test]
    fn never_warmed_service_is_cold() {
        let source = git_source("main");
        assert!(matches!(
            judge(&source, None, false, None),
            Verdict::Cold {
                warmed_at: None,
                ..
            }
        ));
    }

    #[test]
    fn untouched_warm_service_is_warm() {
        let source = git_source("main");
        let at = Utc::now();
        let record = record_for(&source, at);
        assert_eq!(judge(&source, Some(&record), true, None), Verdict::Warm(at));
    }

    #[test]
    fn changing_the_branch_makes_the_service_cold() {
        let record = record_for(&git_source("main"), Utc::now());
        assert!(matches!(
            judge(&git_source("dev"), Some(&record), true, None),
            Verdict::Cold { .. }
        ));
    }

    #[test]
    fn a_new_zip_upload_keeps_the_service_warm() {
        let before = ServiceSource::Archive(ArchiveSource {
            archive_id: "a1".into(),
            ..ArchiveSource::default()
        });
        let after = ServiceSource::Archive(ArchiveSource {
            archive_id: "a2".into(),
            ..ArchiveSource::default()
        });
        assert_eq!(fingerprint(&before), fingerprint(&after));
    }

    #[test]
    fn missing_image_makes_the_service_cold() {
        let source = git_source("main");
        let record = record_for(&source, Utc::now());
        assert!(matches!(
            judge(&source, Some(&record), false, None),
            Verdict::Cold { .. }
        ));
    }

    #[test]
    fn cache_pruned_after_warming_makes_the_service_cold() {
        let source = git_source("main");
        let warmed_at = Utc::now() - chrono::Duration::hours(2);
        let record = record_for(&source, warmed_at);
        let pruned = prune(Utc::now(), 0);
        assert!(matches!(
            judge(&source, Some(&record), true, Some(&pruned)),
            Verdict::Cold { .. }
        ));
    }

    #[test]
    fn cache_pruned_before_warming_keeps_the_service_warm() {
        let source = git_source("main");
        let pruned = prune(Utc::now() - chrono::Duration::hours(3), 0);
        let at = Utc::now();
        let record = record_for(&source, at);
        assert_eq!(
            judge(&source, Some(&record), true, Some(&pruned)),
            Verdict::Warm(at)
        );
    }
}
