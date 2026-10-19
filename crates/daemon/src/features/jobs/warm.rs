//! Aquecimento de jobs: deixar no host as imagens e o cache de build que um job
//! usa, para a próxima execução não baixar nem compilar do zero. Aqui mora a
//! fonte única da verdade de "este job está quente?" (`status`), que as telas
//! só exibem, e o comando que aquece (`request`).
//!
//! "Quente" é um registro (`WarmRecord`, em `daemon_settings`) conferido contra
//! o estado real do Docker: as imagens de registry precisam existir
//! (`inspect_image`) e o cache de build não pode ter sido limpo depois do
//! aquecimento (`docker_prune::last_build_cache_prune`). O registro nasce num
//! aquecimento explícito e também de uma execução bem-sucedida, que baixa e
//! compila tudo do mesmo jeito.

use crate::api::AppState;
use crate::api::handlers::docker_prune::{self, BuildCachePrune};
use crate::db::{self, Db, daemon_settings};
use crate::docker::compose::{self, ComposeImages};
use crate::docker::networks;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared::{Event, Job, JobWarmStatus};
use std::time::Duration;
use tracing::{info, warn};

/// Teto de um aquecimento: ele segura a vaga única da fila de deploy.
const WARM_TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WarmRecord {
    /// Impressão digital da definição do job quando foi aquecido.
    pub fingerprint: String,
    pub warmed_at: DateTime<Utc>,
    pub pulled_images: Vec<String>,
    pub has_build: bool,
}

/// O que identifica "o mesmo job" para o cache: o compose colado, ou o
/// repositório, a branch e o caminho do compose. Mudou, o aquecimento antigo
/// não vale mais. Um commit novo no mesmo repositório NÃO invalida: as imagens
/// base e as camadas de dependências continuam valendo, só as camadas do código
/// novo são refeitas na próxima execução.
pub fn fingerprint(job: &Job) -> String {
    let mut hasher = Sha256::new();
    match &job.git_source {
        Some(git) => {
            hasher.update(b"git\0");
            for part in [&git.url, &git.branch, &git.compose_path] {
                hasher.update(part.as_bytes());
                hasher.update(b"\0");
            }
        }
        None => {
            hasher.update(b"compose\0");
            hasher.update(job.compose.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

fn record_key(job_id: &str) -> String {
    format!("{}{job_id}", daemon_settings::KEY_JOB_WARM_PREFIX)
}

async fn load(db: &Db, job_id: &str) -> Option<WarmRecord> {
    let raw = daemon_settings::get(db, &record_key(job_id))
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

/// Marca o job como aquecido agora, com o que o compose dele precisa ter no host.
pub async fn record(db: &Db, job: &Job, images: &ComposeImages) {
    let record = WarmRecord {
        fingerprint: fingerprint(job),
        warmed_at: Utc::now(),
        pulled_images: images.pulled_images.clone(),
        has_build: images.has_build,
    };
    let Ok(json) = serde_json::to_string(&record) else {
        return;
    };
    if let Err(e) = daemon_settings::set(db, &record_key(&job.id), &json).await {
        warn!(job_id = %job.id, error = %e, "job_warm: falha ao registrar o aquecimento");
    }
}

/// Esquece o aquecimento de um job apagado.
pub async fn forget(db: &Db, job_id: &str) {
    if let Err(e) = daemon_settings::delete(db, &record_key(job_id)).await {
        warn!(job_id = %job_id, error = %e, "job_warm: falha ao apagar o registro de aquecimento");
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

/// O corte de idade da limpeza poupa o cache usado nas últimas N horas: o
/// aquecimento sobreviveu se aconteceu depois do corte (`older_than_hours == 0`
/// limpa tudo, sem corte).
fn cache_pruned_since(warmed_at: DateTime<Utc>, prune: &BuildCachePrune) -> bool {
    if prune.at <= warmed_at {
        return false;
    }
    if prune.older_than_hours == 0 {
        return true;
    }
    let cutoff = prune.at - ChronoDuration::hours(i64::from(prune.older_than_hours));
    warmed_at < cutoff
}

/// A decisão, sem tocar em Docker nem no banco: `missing_image` é a primeira
/// imagem do registro ausente do host e `prune` a última limpeza do cache.
fn judge(
    job: &Job,
    record: Option<&WarmRecord>,
    missing_image: Option<&str>,
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
    if record.fingerprint != fingerprint(job) {
        return cold("a definição do job mudou desde o último aquecimento".to_string());
    }
    if let Some(image) = missing_image {
        return cold(format!("a imagem {image} não está mais no host"));
    }
    if record.has_build
        && let Some(prune) = prune
        && cache_pruned_since(record.warmed_at, prune)
    {
        return cold(format!(
            "o cache de build foi limpo em {}",
            prune.at.format("%d/%m %H:%M UTC")
        ));
    }
    Verdict::Warm(record.warmed_at)
}

/// Fonte única da verdade: se o job está quente agora, e por que não, se frio.
pub async fn status(state: &AppState, job: &Job) -> JobWarmStatus {
    let warming = state
        .warming_jobs
        .lock()
        .map(|jobs| jobs.contains(&job.id))
        .unwrap_or(false);
    let record = load(&state.db, &job.id).await;

    let mut missing_image = None;
    if let Some(record) = &record {
        for image in &record.pulled_images {
            if state.docker.inner.inspect_image(image).await.is_err() {
                missing_image = Some(image.clone());
                break;
            }
        }
    }
    let prune = docker_prune::last_build_cache_prune(&state.db).await;

    match judge(
        job,
        record.as_ref(),
        missing_image.as_deref(),
        prune.as_ref(),
    ) {
        Verdict::Warm(warmed_at) => JobWarmStatus {
            job_id: job.id.clone(),
            warm: true,
            warming,
            reason: None,
            warmed_at: Some(warmed_at),
        },
        Verdict::Cold { reason, warmed_at } => JobWarmStatus {
            job_id: job.id.clone(),
            warm: false,
            warming,
            reason: Some(reason),
            warmed_at,
        },
    }
}

/// Pede o aquecimento de `job`. Devolve `Ok(true)` se ele já estava quente (nada
/// é disparado) e `Ok(false)` se o aquecimento foi para a fila: a vaga única é
/// disputada em segundo plano e o fim chega em `Event::JobWarmChanged`.
pub async fn request(state: &AppState, job: Job) -> Result<bool, String> {
    if status(state, &job).await.warm {
        return Ok(true);
    }
    let newly_warming = state
        .warming_jobs
        .lock()
        .map(|mut jobs| jobs.insert(job.id.clone()))
        .unwrap_or(false);
    if !newly_warming {
        return Err("este job já está sendo aquecido".to_string());
    }
    state.bus.publish(Event::JobWarmChanged {
        job_id: job.id.clone(),
        warming: true,
        success: None,
        error: None,
    });

    let state = state.clone();
    tokio::spawn(async move {
        let permit = state.deploy_queue.acquire_job_slot().await;
        let outcome = tokio::time::timeout(WARM_TIMEOUT, warm_once(&state, &job)).await;
        drop(permit);
        let result = outcome.unwrap_or_else(|_| {
            Err(anyhow!(
                "o aquecimento passou de {}s e foi interrompido",
                WARM_TIMEOUT.as_secs()
            ))
        });

        if let Ok(mut jobs) = state.warming_jobs.lock() {
            jobs.remove(&job.id);
        }
        if let Err(e) = &result {
            warn!(job_id = %job.id, error = %e, "job_warm: falha ao aquecer");
        }
        state.bus.publish(Event::JobWarmChanged {
            job_id: job.id.clone(),
            warming: false,
            success: Some(result.is_ok()),
            error: result.err().map(|e| e.to_string()),
        });
    });
    Ok(false)
}

/// Aquece com a vaga já em mãos. Reconfere o status primeiro: uma execução pode
/// ter aquecido o job enquanto o pedido esperava a vez.
async fn warm_once(state: &AppState, job: &Job) -> Result<()> {
    if status(state, job).await.warm {
        return Ok(());
    }
    info!(job_id = %job.id, "job_warm: aquecendo");

    let project_net = db::projects::network_name(&state.db, &job.project_id).await?;
    let network_name = networks::ensure_project_network(&state.docker.inner, &project_net).await?;
    let env_vars =
        crate::features::deploy::env_resolve::resolve_job(&state.db, &state.secrets, job).await?;

    let project_name = format!("warm-{}", job.id.to_lowercase());
    let build_dir = state.db_path.join("jobs").join(&project_name);
    let result = warm_in_dir(
        state,
        job,
        &project_name,
        &network_name,
        &env_vars,
        &build_dir,
    )
    .await;
    if let Err(e) = tokio::fs::remove_dir_all(&build_dir).await {
        warn!(job_id = %job.id, error = %e, "job_warm: falha ao limpar build_dir (best-effort)");
    }

    record(&state.db, job, &result?).await;
    Ok(())
}

async fn warm_in_dir(
    state: &AppState,
    job: &Job,
    project_name: &str,
    network_name: &str,
    env_vars: &[(String, String)],
    build_dir: &std::path::Path,
) -> Result<ComposeImages> {
    let source = match &job.git_source {
        Some(git) => {
            super::source::clone_git_source(
                &state.db,
                &state.secrets,
                job,
                git,
                build_dir,
                |_progress| {},
            )
            .await?;
            compose::JobBuildSource::Git {
                compose_rel_path: &git.compose_path,
            }
        }
        None => compose::JobBuildSource::Compose(&job.compose),
    };
    compose::warm(
        source,
        project_name,
        network_name,
        env_vars,
        build_dir,
        state.registry_internal_token.clone(),
        &state.docker.inner,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::JobGitSource;

    fn job(compose: &str) -> Job {
        Job {
            id: "job_1".into(),
            project_id: "prj_1".into(),
            trigger_service_id: None,
            name: "migrator".into(),
            compose: compose.into(),
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

    fn warmed_record(job: &Job, warmed_at: DateTime<Utc>, has_build: bool) -> WarmRecord {
        WarmRecord {
            fingerprint: fingerprint(job),
            warmed_at,
            pulled_images: vec!["postgres:18".into()],
            has_build,
        }
    }

    fn prune(at: DateTime<Utc>, older_than_hours: u32) -> BuildCachePrune {
        BuildCachePrune {
            at,
            older_than_hours,
        }
    }

    #[test]
    fn never_warmed_job_is_cold() {
        let job = job("services: {}");
        assert!(matches!(
            judge(&job, None, None, None),
            Verdict::Cold {
                warmed_at: None,
                ..
            }
        ));
    }

    #[test]
    fn untouched_warm_job_is_warm() {
        let job = job("services: {}");
        let at = Utc::now();
        let record = warmed_record(&job, at, true);
        assert_eq!(judge(&job, Some(&record), None, None), Verdict::Warm(at));
    }

    #[test]
    fn editing_the_compose_makes_the_job_cold() {
        let before = job("services: {a: {image: x}}");
        let record = warmed_record(&before, Utc::now(), false);
        let after = job("services: {a: {image: y}}");
        assert!(matches!(
            judge(&after, Some(&record), None, None),
            Verdict::Cold { .. }
        ));
    }

    #[test]
    fn git_fingerprint_follows_repository_branch_and_compose_path_only() {
        let mut git_job = job("");
        git_job.git_source = Some(JobGitSource {
            url: "https://example.com/r.git".into(),
            ..JobGitSource::default()
        });
        let original = fingerprint(&git_job);
        git_job.name = "renomeado".into();
        assert_eq!(fingerprint(&git_job), original);
        git_job.git_source.as_mut().unwrap().branch = "dev".into();
        assert_ne!(fingerprint(&git_job), original);
    }

    #[test]
    fn missing_pulled_image_makes_the_job_cold() {
        let job = job("services: {}");
        let record = warmed_record(&job, Utc::now(), false);
        let verdict = judge(&job, Some(&record), Some("postgres:18"), None);
        let Verdict::Cold { reason, .. } = verdict else {
            panic!("esperava frio");
        };
        assert!(reason.contains("postgres:18"), "{reason}");
    }

    #[test]
    fn full_build_cache_prune_after_warming_makes_a_build_job_cold() {
        let job = job("services: {}");
        let warmed_at = Utc::now() - ChronoDuration::hours(2);
        let record = warmed_record(&job, warmed_at, true);
        let pruned = prune(Utc::now(), 0);
        assert!(matches!(
            judge(&job, Some(&record), None, Some(&pruned)),
            Verdict::Cold { .. }
        ));
    }

    #[test]
    fn build_cache_prune_does_not_touch_a_job_without_build() {
        let job = job("services: {}");
        let warmed_at = Utc::now() - ChronoDuration::hours(2);
        let record = warmed_record(&job, warmed_at, false);
        let pruned = prune(Utc::now(), 0);
        assert!(matches!(
            judge(&job, Some(&record), None, Some(&pruned)),
            Verdict::Warm(_)
        ));
    }

    #[test]
    fn prune_with_age_cutoff_spares_recent_warming_but_not_old() {
        let now = Utc::now();
        let recent = now - ChronoDuration::hours(2);
        let old = now - ChronoDuration::hours(200);
        let pruned = prune(now, 168);
        assert!(
            !cache_pruned_since(recent, &pruned),
            "aquecido dentro do corte sobrevive"
        );
        assert!(
            cache_pruned_since(old, &pruned),
            "aquecido antes do corte é limpo"
        );
    }

    #[test]
    fn prune_that_happened_before_warming_is_irrelevant() {
        let now = Utc::now();
        let pruned = prune(now - ChronoDuration::hours(5), 0);
        assert!(!cache_pruned_since(now, &pruned));
    }
}
