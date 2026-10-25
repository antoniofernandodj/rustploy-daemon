//! Montagem do `rustployd`: config e diretórios, banco, Docker, ingress, `AppState`, recuperação
//! de boot, tarefas de fundo e listeners.

pub mod state;

mod config;
mod listeners;
mod workers;

use crate::api;
use crate::bootstrap::state::AppState;
use crate::engine;
use crate::features::ingress::{IngressController, TlsManager};
use crate::features::{deploy, jobs, registry};
use crate::platform::config::RustployConfig;
use crate::platform::{event_bus::EventBus, secrets};
use crate::store;
use anyhow::Result;
use shared::Event;
use std::{path::PathBuf, sync::Arc};
use tracing::{error, info, warn};

/// Sobe o daemon inteiro e segura o processo (encerramento é o sinal do systemd).
pub async fn run() -> Result<()> {
    let config = RustployConfig::global();

    // `rustployd access-key`: imprime o token de conexão (endereço + token) e sai,
    // sem subir nada. Ver api::agent e docs/plano-chave-de-acesso-e-mcp.md.
    if std::env::args().nth(1).as_deref() == Some("access-key") {
        api::agent::print_access_key_and_exit(config);
    }

    config::init_logging(&config.daemon.log_level);

    // rustls pulls in both `ring` and `aws-lc-rs` providers transitively, so it
    // cannot pick a process-level CryptoProvider on its own and panics on the
    // first TLS use (ACME). Install one explicitly before any TLS work.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring CryptoProvider");

    info!(version = env!("CARGO_PKG_VERSION"), "rustployd starting");
    info!(
        "executando versão {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("RUSTPLOY_GIT_SHA")
    );

    // Database — resolve path with fallback
    let db_path = config::resolve_data_path(&config.daemon.db_path);
    let db = Arc::new(store::connect(&db_path).await?);
    info!("database connected");

    // Directório de backups de env vars: config ou <db_path>/env_backups/
    let backup_dir = config
        .env_backup
        .dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| db_path.join("env_backups"));

    // Docker
    let docker = Arc::new(engine::DockerClient::connect(&config.docker.socket_path)?);
    if let Err(e) = docker.ping().await {
        error!(error = %e, "docker engine unreachable");
        std::process::exit(1);
    }
    info!("docker engine connected");

    let bus = Arc::new(EventBus::new());
    let ingress = Arc::new(IngressController::new());

    let master_key = config::resolve_master_key_path(&config.secrets.master_key_path);
    let secrets = Arc::new(secrets::SecretsManager::new(&master_key, db.clone())?);

    // TLS / ACME — email do banco tem precedência sobre o config
    let acme_config = {
        let mut acme = config.ingress.acme.clone();
        if let Ok(Some(email)) =
            store::daemon_settings::get(&db, store::daemon_settings::KEY_ACME_EMAIL).await
            && !email.trim().is_empty()
        {
            info!(email = %email, "ACME: usando email do banco de dados");
            acme.email = Some(email);
            acme.enabled = true;
        }
        acme
    };
    let certs_dir = config::resolve_data_path(&config.daemon.db_path).join("certs");
    let tls = Arc::new(
        TlsManager::new(certs_dir, acme_config.clone()).expect("failed to initialize TLS manager"),
    );

    // Token interno usado pelo deploy executor pra puxar imagens do próprio
    // registry embutido, sem ação manual do usuário (Fase 3 do plano do
    // registry). Regenerado a cada boot (ver db/registry_tokens.rs) — não
    // pode ser fatal: um erro aqui não deve derrubar o boot do daemon inteiro.
    let registry_internal_token = if config.registry.enabled {
        match registry::internal_token::ensure(&db).await {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(error = %e, "registry: falha ao gerar token interno, pull de imagens do registry embutido vai falhar até o próximo restart");
                None
            }
        }
    } else {
        None
    };

    // Grava o nome da stack Compose dos serviços que ainda não têm, com o que já
    // está no Docker (nada muda de nome). Antes da recovery, que já lê o gravado.
    match engine::containers::list_compose_projects(&docker.inner).await {
        Ok(live) => {
            if let Err(e) = store::services::backfill_compose_projects(&db, &live).await {
                error!(error = %e, "falha ao gravar o nome das stacks Compose");
            }
        }
        Err(e) => {
            warn!(error = %e, "não listei as stacks Compose; nomes seguem pela fórmula legada")
        }
    }

    // Recovery — devolve os deploys que estavam na fila (Pending) para
    // re-enfileirar depois que o AppState/DeployQueue existir.
    let deploys_to_requeue = deploy::recovery::recover(
        db.clone(),
        docker.clone(),
        ingress.clone(),
        bus.clone(),
        secrets.clone(),
        tls.clone(),
        db_path.clone(),
        config.deploy.drain_secs,
        registry_internal_token.clone(),
    )
    .await;

    workers::spawn_standalone(config, &db, &docker, &bus, &backup_dir);

    bus.publish(Event::DaemonReady {
        version: env!("CARGO_PKG_VERSION").to_string(),
    });

    let registry_storage = listeners::prepare_registry_storage(config, &db_path);
    if registry_storage.is_some() {
        listeners::publish_registry_route(config, &db, &ingress, &tls).await;
    }

    let state = AppState::new(
        db,
        docker,
        ingress.clone(),
        bus,
        secrets,
        tls.clone(),
        db_path,
        backup_dir,
        config.deploy.drain_secs,
        config.api.clone(),
        &config.update,
        registry_storage.clone(),
        registry_internal_token.clone(),
    );

    // Execuções de job que ficaram sem fim num restart (rodando, ou esperando a
    // vez numa fila que só existia em memória): viram "interrompidas". Antes do
    // agendador, que só dispara um job sem execução em andamento.
    match jobs::recover_interrupted(&state.db).await {
        Ok(0) => {}
        Ok(n) => info!(
            n,
            "execuções de job interrompidas pelo restart foram fechadas"
        ),
        Err(e) => warn!(error = %e, "falha ao fechar execuções de job interrompidas"),
    }

    // Re-enfileira os deploys que estavam na fila antes do restart (na ordem de criação
    // devolvida pelo recovery); o worker é spawnado em `workers::spawn_stateful`.
    for dep_id in deploys_to_requeue {
        state.deploy_queue.enqueue(dep_id);
    }

    // Migração deixada "Running" por um daemon que caiu: não retoma, vira Failed.
    if let Ok(n) = store::migration::fail_orphans(&state.db).await
        && n > 0
    {
        tracing::warn!(
            n,
            "migrações interrompidas pelo restart marcadas como Failed"
        );
    }

    workers::spawn_stateful(&state, registry_storage.clone(), &tls);
    listeners::spawn(config, &state, &ingress, &tls, registry_storage);

    // Todo listener do daemon roda em `tokio::spawn`, então sem isto a
    // `main` retornaria e o processo morreria no boot: este pending existe só
    // para segurar o processo. Encerramento é o sinal do systemd (SIGTERM).
    std::future::pending::<()>().await;
    Ok(())
}
