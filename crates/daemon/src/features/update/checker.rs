//! `UpdateChecker`: compara a versão do daemon com a última release, com cache.
//!
//! O cache existe por causa do limite da API anônima do GitHub (60 consultas/h por
//! IP): sem ele, abrir a tela de Settings em alguns navegadores esgotaria a cota.
//! O lock fica preso durante a consulta de propósito: chamadas simultâneas esperam
//! e reaproveitam o resultado em vez de cada uma ir ao GitHub.

use super::github_release::LatestRelease;
use crate::platform::config::UpdateConfig;
use anyhow::{Result, anyhow};
use chrono::Utc;
use shared::UpdateInfo;
use std::{future::Future, time::Duration};
use tokio::{sync::Mutex, time::Instant};
use update_core::{is_newer, parse_version};

/// Uma consulta *forçada* ("Verificar agora") ainda respeita este intervalo mínimo:
/// um botão clicado em sequência não pode queimar a cota do GitHub.
const FORCE_MIN_SPACING: Duration = Duration::from_secs(60);

struct Cached {
    fetched_at: Instant,
    info: UpdateInfo,
}

pub struct UpdateChecker {
    enabled: bool,
    repo: String,
    ttl: Duration,
    force_spacing: Duration,
    cached: Mutex<Option<Cached>>,
}

impl UpdateChecker {
    pub fn new(config: &UpdateConfig) -> Self {
        Self::with_timings(
            config,
            Duration::from_secs(config.cache_hours * 3600),
            FORCE_MIN_SPACING,
        )
    }

    fn with_timings(config: &UpdateConfig, ttl: Duration, force_spacing: Duration) -> Self {
        Self {
            enabled: config.enabled,
            repo: config.repo.clone(),
            ttl,
            force_spacing,
            cached: Mutex::new(None),
        }
    }

    /// Consulta o GitHub de verdade (a menos que o cache sirva).
    pub async fn check(&self, force: bool) -> Result<UpdateInfo> {
        let repo = self.repo.clone();
        self.check_with(force, env!("CARGO_PKG_VERSION"), move || {
            super::github_release::fetch_latest_owned(repo)
        })
        .await
    }

    async fn check_with<Fetch, FetchFuture>(
        &self,
        force: bool,
        current_version: &str,
        fetch: Fetch,
    ) -> Result<UpdateInfo>
    where
        Fetch: FnOnce() -> FetchFuture,
        FetchFuture: Future<Output = Result<LatestRelease>>,
    {
        if !self.enabled {
            return Err(anyhow!(
                "a verificação de atualizações está desligada (`enabled = false` em [update])"
            ));
        }

        let mut cached = self.cached.lock().await;
        if let Some(entry) = cached.as_ref() {
            let age = entry.fetched_at.elapsed();
            let still_valid = age < self.ttl;
            let too_soon_to_force = age < self.force_spacing;
            if (!force && still_valid) || (force && too_soon_to_force) {
                return Ok(entry.info.clone());
            }
        }

        let release = fetch().await?;
        let info = build_info(current_version, &release)?;
        *cached = Some(Cached {
            fetched_at: Instant::now(),
            info: info.clone(),
        });
        Ok(info)
    }
}

fn build_info(current_version: &str, release: &LatestRelease) -> Result<UpdateInfo> {
    let latest = parse_version(&release.tag).map_err(|_| {
        anyhow!(
            "a tag da última release ({:?}) não é uma versão",
            release.tag
        )
    })?;
    let is_update_available = is_newer(&release.tag, current_version)
        .map_err(|error| anyhow!("não consegui comparar com a versão do daemon: {error}"))?;
    Ok(UpdateInfo {
        current_version: current_version.to_string(),
        latest_version: latest.to_string(),
        is_update_available,
        release_url: release.url.clone(),
        published_at: release.published_at,
        checked_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn release(tag: &str) -> LatestRelease {
        LatestRelease {
            tag: tag.into(),
            url: format!("https://github.com/o/r/releases/tag/{tag}"),
            published_at: None,
        }
    }

    fn checker(ttl: Duration, force_spacing: Duration) -> UpdateChecker {
        UpdateChecker::with_timings(&UpdateConfig::default(), ttl, force_spacing)
    }

    #[tokio::test]
    async fn newer_release_is_reported_with_numeric_comparison() {
        let info = checker(Duration::ZERO, Duration::ZERO)
            .check_with(false, "0.1.9", || async { Ok(release("v0.1.15")) })
            .await
            .unwrap();
        assert!(info.is_update_available);
        assert_eq!(info.latest_version, "0.1.15");
        assert_eq!(info.current_version, "0.1.9");
    }

    #[tokio::test]
    async fn same_or_older_release_is_not_an_update() {
        for tag in ["v0.1.7", "v0.1.2"] {
            let info = checker(Duration::ZERO, Duration::ZERO)
                .check_with(false, "0.1.7", || async { Ok(release(tag)) })
                .await
                .unwrap();
            assert!(!info.is_update_available, "{tag}");
        }
    }

    #[tokio::test]
    async fn a_non_version_tag_is_an_error() {
        let error = checker(Duration::ZERO, Duration::ZERO)
            .check_with(false, "0.1.7", || async { Ok(release("nightly")) })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("nightly"), "{error}");
    }

    #[tokio::test]
    async fn cache_serves_repeat_calls_without_asking_github_again() {
        let calls = AtomicUsize::new(0);
        let checker = checker(Duration::from_secs(3600), Duration::from_secs(60));
        for _ in 0..3 {
            checker
                .check_with(false, "0.1.7", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(release("v0.1.8"))
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn force_is_rate_limited_but_allowed_after_the_spacing() {
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(release("v0.1.8"))
        };

        // Espaçamento grande: o segundo "forçar" volta do cache.
        let throttled = checker(Duration::from_secs(3600), Duration::from_secs(3600));
        throttled.check_with(true, "0.1.7", fetch).await.unwrap();
        throttled.check_with(true, "0.1.7", fetch).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Espaçamento zero: forçar sempre consulta de novo.
        calls.store(0, Ordering::SeqCst);
        let open = checker(Duration::from_secs(3600), Duration::ZERO);
        open.check_with(true, "0.1.7", fetch).await.unwrap();
        open.check_with(true, "0.1.7", fetch).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn expired_cache_asks_again() {
        let calls = AtomicUsize::new(0);
        let checker = checker(Duration::ZERO, Duration::ZERO);
        for _ in 0..2 {
            checker
                .check_with(false, "0.1.7", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(release("v0.1.8"))
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_fetch_is_an_error_and_is_not_cached() {
        let checker = checker(Duration::from_secs(3600), Duration::ZERO);
        assert!(
            checker
                .check_with(false, "0.1.7", || async { Err(anyhow!("sem rede")) })
                .await
                .is_err()
        );
        // O erro não "gruda": a próxima tentativa vai ao GitHub.
        let info = checker
            .check_with(false, "0.1.7", || async { Ok(release("v0.1.8")) })
            .await
            .unwrap();
        assert!(info.is_update_available);
    }

    #[tokio::test]
    async fn disabled_never_calls_github() {
        let config = UpdateConfig {
            enabled: false,
            ..UpdateConfig::default()
        };
        let checker = UpdateChecker::with_timings(&config, Duration::ZERO, Duration::ZERO);
        let error = checker
            .check_with(true, "0.1.7", || async {
                panic!("não deveria consultar o GitHub");
                #[allow(unreachable_code)]
                Ok(release("v9.9.9"))
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("desligada"), "{error}");
    }
}
