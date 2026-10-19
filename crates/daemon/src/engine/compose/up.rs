//! `compose up` de uma stack de serviço: login no registry, rede do projeto e volumes externos.

use crate::platform::config::RustployConfig;
use crate::platform::{event_bus::EventBus, log_sink::LogSink};
use anyhow::{Result, anyhow};
use bollard::{Docker, volume::CreateVolumeOptions};
use chrono::Utc;
use shared::Event;
use std::path::Path;
use std::sync::Arc;
use tokio::fs::{File, remove_file};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::info;

pub(super) const PROJECT_NET_ALIAS: &str = "rp_project_net";

/// Loga no registry embutido com o token interno `rp-internal` antes de um
/// `docker compose up` (que pode dar pull de imagens de lá). Diferente do
/// pull via bollard em `docker/images.rs` (credenciais passadas por chamada),
/// o `docker compose` é um subprocesso CLI que só entende credenciais via
/// `~/.docker/config.json` — por isso precisa de login/logout explícitos.
/// Best-effort: falha aqui não impede a tentativa de `up` (só vai falhar o
/// pull depois, com erro visível nos logs, se a imagem realmente for da lá).
pub(super) async fn registry_login(token: &str) -> Result<()> {
    let port = RustployConfig::global().registry.port;
    let mut child = crate::engine::cli()
        .args([
            "login",
            &format!("127.0.0.1:{port}"),
            "-u",
            "rp-internal",
            "--password-stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("falha ao iniciar docker login: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(token.as_bytes()).await.ok();
        stdin.shutdown().await.ok();
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| anyhow!("falha ao aguardar docker login: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker login falhou: {stderr}"));
    }
    Ok(())
}

/// Desloga do registry embutido (limpeza best-effort, chamada sempre que
/// `registry_login` foi tentado, mesmo se o `up` falhou).
pub(super) async fn registry_logout() -> Result<()> {
    let port = RustployConfig::global().registry.port;
    let status = crate::engine::cli()
        .args(["logout", &format!("127.0.0.1:{port}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| anyhow!("falha ao iniciar docker logout: {e}"))?;
    if !status.success() {
        return Err(anyhow!("docker logout falhou"));
    }
    Ok(())
}

/// Reescreve o YAML do Compose para que todo serviço entre também na rede do
/// projeto (declarada como `external`), sem perder as redes que o usuário já
/// declarou.
pub fn inject_project_network(content: &str, network_name: &str) -> Result<String> {
    use serde_yaml::Value;
    let mut doc: Value =
        serde_yaml::from_str(content).map_err(|e| anyhow!("compose YAML inválido: {e}"))?;

    if doc.is_null() {
        doc = Value::Mapping(serde_yaml::Mapping::new());
    }
    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("compose YAML não é um mapping no nível raiz"))?;

    let alias_key = Value::String(PROJECT_NET_ALIAS.to_string());
    let networks_key = Value::String("networks".to_string());
    if !root.contains_key(&networks_key) {
        root.insert(
            networks_key.clone(),
            Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let networks = root
        .get_mut(&networks_key)
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| anyhow!("`networks:` no compose não é um mapping"))?;

    let already_present = networks.contains_key(&alias_key)
        || networks.values().any(|v| {
            v.as_mapping()
                .and_then(|m| m.get(Value::String("name".to_string())))
                .and_then(Value::as_str)
                == Some(network_name)
        });

    if !already_present {
        let mut entry = serde_yaml::Mapping::new();
        entry.insert(Value::String("external".to_string()), Value::Bool(true));
        entry.insert(
            Value::String("name".to_string()),
            Value::String(network_name.to_string()),
        );
        networks.insert(alias_key.clone(), Value::Mapping(entry));
    }
    let services_key = Value::String("services".to_string());
    if let Some(services_val) = root.get_mut(&services_key) {
        let services = services_val
            .as_mapping_mut()
            .ok_or_else(|| anyhow!("`services:` no compose não é um mapping"))?;

        let svc_net_key = Value::String("networks".to_string());
        for (_, svc) in services.iter_mut() {
            let Some(svc_map) = svc.as_mapping_mut() else {
                continue;
            };
            match svc_map.get_mut(&svc_net_key) {
                Some(Value::Sequence(seq)) => {
                    if !seq.iter().any(|v| v.as_str() == Some(PROJECT_NET_ALIAS)) {
                        seq.push(Value::String(PROJECT_NET_ALIAS.to_string()));
                    }
                }
                Some(Value::Mapping(map)) => {
                    if !map.contains_key(&alias_key) {
                        map.insert(
                            alias_key.clone(),
                            Value::Mapping(serde_yaml::Mapping::new()),
                        );
                    }
                }
                Some(other) if other.is_null() => {
                    *other = Value::Sequence(vec![Value::String(PROJECT_NET_ALIAS.to_string())]);
                }
                Some(_) => {}
                None => {
                    svc_map.insert(
                        svc_net_key.clone(),
                        Value::Sequence(vec![Value::String(PROJECT_NET_ALIAS.to_string())]),
                    );
                }
            }
        }
    }
    serde_yaml::to_string(&doc).map_err(|e| anyhow!("falha ao serializar compose YAML: {e}"))
}

/// Garante que todo volume declarado `external: true` no compose já exista no
/// Docker antes do `up` — o Compose se recusa a criar volumes externos, então
/// se o volume tiver sido removido por fora (ex.: prune manual) o deploy
/// falharia sempre com "external volume ... not found". Idempotente: só cria
/// o que estiver faltando, nunca mexe em volume já existente. Mesmo idioma de
/// `networks::ensure_project_network` (ensure-then-create), aplicado a volumes.
pub async fn ensure_external_volumes(docker: &Docker, content: &str) -> Result<()> {
    use serde_yaml::Value;

    let doc: Value =
        serde_yaml::from_str(content).map_err(|e| anyhow!("compose YAML inválido: {e}"))?;

    let Some(volumes) = doc.get("volumes").and_then(Value::as_mapping) else {
        return Ok(());
    };

    for (key, def) in volumes {
        let Some(map) = def.as_mapping() else {
            continue;
        };

        let is_external = map
            .get(Value::String("external".to_string()))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_external {
            continue;
        }

        let name = map
            .get(Value::String("name".to_string()))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| key.as_str().map(str::to_string));
        let Some(name) = name else {
            continue;
        };

        if docker.inspect_volume(&name).await.is_ok() {
            info!(volume = %name, "compose::ensure_external_volumes: volume já existe");
            continue;
        }

        info!(volume = %name, "compose::ensure_external_volumes: criando volume externo ausente");
        docker
            .create_volume(CreateVolumeOptions {
                name: name.clone(),
                ..Default::default()
            })
            .await?;
    }

    Ok(())
}

/// Sobe (ou atualiza) a stack Compose de um serviço: grava `.env` e
/// `docker-compose.yml` com a rede do projeto injetada e roda `docker compose
/// up -d`, com o progresso no log de build.
#[allow(clippy::too_many_arguments)]
pub async fn up(
    docker: &Docker,
    content: &str,
    project_name: &str,
    service_id: &str,
    deployment_id: &str,
    network_name: &str,
    bus: &Arc<EventBus>,
    db: &Arc<impl LogSink>,
    env_vars: &[(String, String)],
    build_dir: &Path,
    registry_internal_token: Option<Arc<str>>,
) -> Result<()> {
    info!(project = %project_name, "compose_up: iniciando docker compose up");

    let content = inject_project_network(content, network_name)?;
    ensure_external_volumes(docker, &content).await?;

    // Garantir diretório
    tokio::fs::create_dir_all(build_dir).await?;

    // Criar arquivos .env e docker-compose.yml
    let env_file_path = build_dir.join(".env");
    let compose_file_path = build_dir.join("docker-compose.yml");

    {
        let mut env_file = File::create(&env_file_path).await?;
        for (k, v) in env_vars {
            env_file
                .write_all(format!("{}={}\n", k, v).as_bytes())
                .await?;
        }
        env_file.flush().await?;
    } // Aqui o arquivo é fechado

    let mut compose_file = File::create(&compose_file_path).await?;
    compose_file.write_all(content.as_bytes()).await?;
    compose_file.flush().await?;
    drop(compose_file);

    if let Some(token) = &registry_internal_token
        && let Err(e) = registry_login(token).await
    {
        tracing::warn!(error = %e, project = %project_name, "compose_up: falha ao autenticar no registry embutido, pull vai falhar se a imagem for de lá");
    }

    let mut child = crate::engine::cli()
        .args([
            "compose",
            "-p",
            project_name,
            "-f",
            "docker-compose.yml",
            "--env-file",
            ".env",
            "up",
            "-d",
            "--build",
            "--remove-orphans",
        ])
        .current_dir(build_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Abortar o deploy derruba a task; sem isto o `compose up --build`
        // seguiria vivo no host.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("falha ao iniciar docker compose: {e}"))?;

    let stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr = BufReader::new(child.stderr.take().unwrap());

    let bus_s = bus.clone();
    let db_s = db.clone();
    let sid = service_id.to_string();
    let did = deployment_id.to_string();
    let read_stdout = async move {
        let mut lines = stdout.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let ts = Utc::now();
            bus_s.publish(Event::BuildLog {
                deployment_id: did.clone(),
                service_id: sid.clone(),
                line: line.clone(),
                timestamp: ts,
            });
            let _ = db_s.append_build_line(&did, &line, ts).await;
        }
    };
    let bus_e = bus.clone();
    let db_e = db.clone();
    let sid_e = service_id.to_string();
    let did_e = deployment_id.to_string();
    let read_stderr = async move {
        let mut lines = stderr.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let ts = Utc::now();
            bus_e.publish(Event::BuildLog {
                deployment_id: did_e.clone(),
                service_id: sid_e.clone(),
                line: line.clone(),
                timestamp: ts,
            });
            let _ = db_e.append_build_line(&did_e, &line, ts).await;
        }
    };

    tokio::join!(read_stdout, read_stderr);
    let status = child
        .wait()
        .await
        .map_err(|e| anyhow!("falha ao aguardar: {e}"))?;

    if registry_internal_token.is_some()
        && let Err(e) = registry_logout().await
    {
        tracing::warn!(error = %e, project = %project_name, "compose_up: falha ao deslogar do registry embutido (best-effort)");
    }

    // Limpeza
    let _ = remove_file(&env_file_path).await;
    let _ = remove_file(&compose_file_path).await;

    if !status.success() {
        return Err(anyhow!("docker compose up falhou"));
    }
    Ok(())
}
