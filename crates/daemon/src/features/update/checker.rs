//! `UpdateChecker`: compara a versão do daemon com a última release, com cache.
//!
//! O cache existe por causa do limite da API anônima do GitHub (60 consultas/h por
//! IP): sem ele, abrir a tela de Settings em alguns navegadores esgotaria a cota.
//! O lock fica preso durante a consulta de propósito: chamadas simultâneas esperam
//! e reaproveitam o resultado em vez de cada uma ir ao GitHub.
//!
//! [`periodic_loop`](UpdateChecker::periodic_loop) consulta sozinho de tempos em tempos e
//! o snapshot das UIs lê o resultado de [`cached_info`](UpdateChecker::cached_info), para o
//! selo de versão nova acender numa janela aberta sem ninguém clicar em "Verificar".

use super::github_release::LatestRelease;
use crate::platform::config::UpdateConfig;
use anyhow::{Result, anyhow};
use chrono::Utc;
use shared::UpdateInfo;
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};
use update_core::{
    is_newer,
    release::{debian_architecture, package_version},
};

/// Uma consulta *forçada* ("Verificar agora") ainda respeita este intervalo mínimo:
/// um botão clicado em sequência não pode queimar a cota do GitHub.
const FORCE_MIN_SPACING: Duration = Duration::from_secs(60);

/// A primeira consulta automática espera o daemon terminar de subir (e não disputa a
/// rede com a recuperação de deploys logo no boot).
const PERIODIC_FIRST_DELAY: Duration = Duration::from_secs(60);

struct Cached {
    fetched_at: Instant,
    info: UpdateInfo,
    /// A release inteira (com a lista de arquivos), de onde `ApplyUpdate` tira o pacote.
    release: LatestRelease,
}

pub struct UpdateChecker {
    enabled: bool,
    repo: String,
    ttl: Duration,
    force_spacing: Duration,
    /// Zero: sem consulta automática.
    check_interval: Duration,
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
            check_interval: Duration::from_secs(config.check_interval_hours * 3600),
            cached: Mutex::new(None),
        }
    }

    /// A última release vista por [`check`](Self::check), com os arquivos dela.
    pub async fn cached_release(&self) -> Option<LatestRelease> {
        self.cached
            .lock()
            .await
            .as_ref()
            .map(|entry| entry.release.clone())
    }

    /// O último resultado, sem ir ao GitHub: o snapshot de 2 s das UIs lê daqui.
    pub async fn cached_info(&self) -> Option<UpdateInfo> {
        self.cached
            .lock()
            .await
            .as_ref()
            .map(|entry| entry.info.clone())
    }

    /// Consulta o GitHub a cada `check_interval_hours` (`[update]`). Não roda com a verificação
    /// desligada nem com o intervalo `0`. A consulta é forçada para não depender do `cache_hours`;
    /// uma falha só vai para o log e a próxima volta tenta de novo.
    pub async fn periodic_loop(self: Arc<Self>) {
        if !self.enabled || self.check_interval.is_zero() {
            return;
        }
        tokio::time::sleep(PERIODIC_FIRST_DELAY).await;
        let mut interval = tokio::time::interval(self.check_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match self.check(true).await {
                Ok(info) if info.is_update_available => tracing::info!(
                    current = %info.current_version,
                    latest = %info.latest_version,
                    "update: há versão nova do daemon"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(error = %error, "update: falha na consulta periódica"),
            }
        }
    }

    /// O repositório consultado (`dono/nome`); `ApplyUpdate` só baixa de URLs dele.
    pub fn repo(&self) -> &str {
        &self.repo
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
        let info = build_info(current_version, &release, &self.repo)?;
        *cached = Some(Cached {
            fetched_at: Instant::now(),
            info: info.clone(),
            release,
        });
        Ok(info)
    }
}

/// Compara o que roda com o daemon que a release **traz** (a versão do pacote `rustployd_<v>_<arq>.deb`),
/// e não com a tag: a tag da release (`v0.1.18`) e a versão do daemon (`0.1.12`) são numerações
/// independentes, e comparar as duas dizia "há versão nova" para sempre.
fn build_info(current_version: &str, release: &LatestRelease, repo: &str) -> Result<UpdateInfo> {
    let architecture = debian_architecture().map_err(|error| anyhow!(error))?;
    let package =
        package_version(&release.assets, repo, "rustployd", architecture).ok_or_else(|| {
            anyhow!(
                "a release {} não traz um pacote do daemon para {architecture}",
                release.tag
            )
        })?;
    let is_update_available = is_newer(&package.upstream, current_version)
        .map_err(|error| anyhow!("não consegui comparar com a versão do daemon: {error}"))?;
    Ok(UpdateInfo {
        current_version: current_version.to_string(),
        latest_version: package.upstream,
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

    /// Uma release cuja tag é `tag` e cujo daemon tem a MESMA versão que a tag (`v0.1.8` traz
    /// `rustployd_0.1.8-1_<arq>.deb`), o caso simples. Para numerações diferentes, `release_with`.
    fn release(tag: &str) -> LatestRelease {
        release_with(tag, tag.trim_start_matches('v'))
    }

    fn release_with(tag: &str, daemon_version: &str) -> LatestRelease {
        let architecture = debian_architecture().unwrap();
        let name = format!("rustployd_{daemon_version}-1_{architecture}.deb");
        LatestRelease {
            tag: tag.into(),
            url: format!("https://github.com/o/r/releases/tag/{tag}"),
            published_at: None,
            assets: vec![update_core::release::ReleaseAsset {
                url: format!(
                    "https://github.com/{}/releases/download/{tag}/{name}",
                    UpdateConfig::default().repo
                ),
                name,
            }],
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
    async fn the_release_tag_does_not_matter_only_the_daemon_package_does() {
        // A tag é v0.1.18, mas o daemon que ela traz é o 0.1.12 (numerações independentes).
        // Quem já roda o 0.1.12 está em dia; quem roda o 0.1.11 tem atualização.
        let up_to_date = checker(Duration::ZERO, Duration::ZERO)
            .check_with(false, "0.1.12", || async {
                Ok(release_with("v0.1.18", "0.1.12"))
            })
            .await
            .unwrap();
        assert!(!up_to_date.is_update_available);
        assert_eq!(up_to_date.latest_version, "0.1.12");

        let behind = checker(Duration::ZERO, Duration::ZERO)
            .check_with(false, "0.1.11", || async {
                Ok(release_with("v0.1.18", "0.1.12"))
            })
            .await
            .unwrap();
        assert!(behind.is_update_available);
        assert_eq!(behind.latest_version, "0.1.12");
    }

    #[tokio::test]
    async fn a_release_without_a_daemon_package_is_an_error_not_an_update() {
        let mut only_the_app = release_with("v0.1.20", "0.1.20");
        only_the_app.assets.clear();
        let error = checker(Duration::ZERO, Duration::ZERO)
            .check_with(false, "0.1.7", || async { Ok(only_the_app) })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("não traz um pacote do daemon"), "{error}");
    }

    #[tokio::test]
    async fn a_package_from_another_repository_does_not_count() {
        let mut hostile = release_with("v9.9.9", "9.9.9");
        hostile.assets[0].url = "https://evil.example/rustployd_9.9.9-1_amd64.deb".into();
        let error = checker(Duration::ZERO, Duration::ZERO)
            .check_with(false, "0.1.7", || async { Ok(hostile) })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("não traz um pacote do daemon"), "{error}");
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
    async fn cached_info_never_asks_github_and_follows_the_last_check() {
        let checker = checker(Duration::from_secs(3600), Duration::ZERO);
        assert!(checker.cached_info().await.is_none());
        checker
            .check_with(false, "0.1.7", || async { Ok(release("v0.1.8")) })
            .await
            .unwrap();
        let info = checker.cached_info().await.unwrap();
        assert!(info.is_update_available);
        assert_eq!(info.latest_version, "0.1.8");
    }

    #[tokio::test]
    async fn periodic_loop_does_not_run_when_disabled_or_interval_is_zero() {
        for config in [
            UpdateConfig {
                enabled: false,
                ..UpdateConfig::default()
            },
            UpdateConfig {
                check_interval_hours: 0,
                ..UpdateConfig::default()
            },
        ] {
            // Um laço ligado dormiria o `PERIODIC_FIRST_DELAY` inteiro: voltar logo é a prova.
            let finished = tokio::time::timeout(
                Duration::from_secs(1),
                Arc::new(UpdateChecker::new(&config)).periodic_loop(),
            )
            .await;
            assert!(finished.is_ok(), "{config:?}");
        }
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
