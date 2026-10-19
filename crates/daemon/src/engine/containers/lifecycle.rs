//! Ciclo de vida de um container: criar o staging, start, stop, rename, remover e alias de rede.

use anyhow::{Result, anyhow};
use bollard::{
    Docker,
    container::{
        Config, CreateContainerOptions, InspectContainerOptions, NetworkingConfig,
        RemoveContainerOptions, RenameContainerOptions, StartContainerOptions,
        StopContainerOptions,
    },
    errors::Error as BollardError,
    models::{
        EndpointSettings, HostConfig, Mount, MountTypeEnum, RestartPolicy, RestartPolicyNameEnum,
    },
};
use shared::ServiceSpec;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Dá ao container o alias `alias` na rede `network`, reconectando-o
/// (`disconnect` + `connect`): o Docker não altera aliases de um endpoint
/// existente. **O IP do container na rede muda** — quem guardou o IP (ingress)
/// precisa relê-lo. Não faz nada se o alias já estiver lá (retomada de deploy
/// pela recovery), para não derrubar conexões à toa.
pub async fn attach_network_alias(
    docker: &Docker,
    container_id: &str,
    network: &str,
    alias: &str,
) -> Result<()> {
    use bollard::network::{ConnectNetworkOptions, DisconnectNetworkOptions};
    let info = docker
        .inspect_container(container_id, None::<InspectContainerOptions>)
        .await?;
    let ja_tem = info
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
        .and_then(|n| n.get(network))
        .and_then(|e| e.aliases.as_ref())
        .is_some_and(|a| a.iter().any(|x| x == alias));
    if ja_tem {
        debug!(container_id = %container_id, alias = %alias, "alias já presente");
        return Ok(());
    }
    docker
        .disconnect_network(
            network,
            DisconnectNetworkOptions {
                container: container_id.to_string(),
                force: true,
            },
        )
        .await
        .map_err(|e| anyhow!("falha ao desconectar {container_id} de {network}: {e}"))?;
    docker
        .connect_network(
            network,
            ConnectNetworkOptions {
                container: container_id.to_string(),
                endpoint_config: EndpointSettings {
                    aliases: Some(vec![alias.to_string()]),
                    ..Default::default()
                },
            },
        )
        .await
        .map_err(|e| {
            anyhow!("falha ao reconectar {container_id} a {network} com alias {alias}: {e}")
        })?;
    info!(container_id = %container_id, network = %network, alias = %alias, "alias de rede atribuído");
    Ok(())
}

/// Cria o container de staging de uma réplica já na rede do projeto
/// (`network_mode` e `NetworkingConfig`, como o `docker run --network`), com
/// labels, env, volumes e limites do spec.
#[allow(clippy::too_many_arguments)]
pub async fn create_staging(
    docker: &Docker,
    spec: &ServiceSpec,
    image: &str,
    service_id: &str,
    deployment_id: &str,
    network_id: &str,
    resolved_env: &[(String, String)],
    container_name: &str,
    replica: u32,
) -> Result<String> {
    let name = container_name;
    info!(
        name = %name,
        image = %image,
        network = %network_id,
        service_id = %service_id,
        port = spec.port,
        volumes = spec.volumes.len(),
        "criando container"
    );

    let env: Vec<String> = resolved_env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    debug!(
        name = %name,
        env_keys = ?resolved_env
            .iter()
            .map(
                |(k, _)| k.as_str()
            )
            .collect::<Vec<_>>(),
        "env vars configuradas"
    );

    let mounts: Vec<Mount> = spec
        .volumes
        .iter()
        .map(|v| {
            debug!(
                host = %v.host_path,
                container = %v.container_path,
                ro = v.read_only,
                "montando volume"
            );
            Mount {
                target: Some(v.container_path.clone()),
                source: Some(v.host_path.clone()),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(v.read_only),
                ..Default::default()
            }
        })
        .collect();

    let mut labels = HashMap::new();
    labels.insert("rustploy.managed".to_string(), "true".to_string());

    labels.insert("rustploy.service_id".to_string(), service_id.to_string());

    labels.insert(
        "rustploy.deployment_id".to_string(),
        deployment_id.to_string(),
    );

    labels.insert("rustploy.replica".to_string(), replica.to_string());

    let mem_limit = if spec.resources.mem_limit_bytes > 0 {
        Some(spec.resources.mem_limit_bytes as i64)
    } else {
        None
    };
    let cpu_shares = if spec.resources.cpu_shares > 0 {
        Some(spec.resources.cpu_shares as i64)
    } else {
        None
    };

    debug!(
        name = %name,
        mem_limit = ?mem_limit,
        cpu_shares = ?cpu_shares,
        "limites de recurso"
    );

    let host_config = HostConfig {
        // network_mode substitui a bridge padrão pela rede user-defined do projeto.
        // Equivalente a `docker run --network <rede>`: o Docker configura o DNS
        // embebido (127.0.0.11) imediatamente, sem depender de network connect.
        network_mode: Some(network_id.to_string()),
        mounts: Some(mounts),
        memory: mem_limit,
        cpu_shares,
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::NO),
            maximum_retry_count: None,
        }),
        ..Default::default()
    };

    let cmd = if spec.run_command.is_some() || !spec.run_args.is_empty() {
        let mut parts: Vec<String> = spec
            .run_command
            .as_deref()
            .map(|c| vec![c.to_string()])
            .unwrap_or_default();
        parts.extend(spec.run_args.iter().cloned());
        Some(parts)
    } else {
        None
    };

    // Replica o comportamento de `docker run --network <rede>`:
    // a CLI envia TANTO network_mode no HostConfig COMO o endpoint em NetworkingConfig.
    // Sem NetworkingConfig, o Docker pode não configurar o DNS embebido (127.0.0.11)
    // correctamente mesmo com network_mode definido.
    let mut endpoints = HashMap::new();
    endpoints.insert(network_id.to_string(), EndpointSettings::default());

    let config = Config {
        image: Some(image.to_string()),
        env: Some(env),
        labels: Some(labels),
        host_config: Some(host_config),
        cmd,
        exposed_ports: Some({
            let mut m = HashMap::new();
            m.insert(format!("{}/tcp", spec.port), HashMap::new());
            m
        }),
        networking_config: Some(NetworkingConfig {
            endpoints_config: endpoints,
        }),
        ..Default::default()
    };

    let opts = CreateContainerOptions {
        name,
        platform: None,
    };
    let response = docker.create_container(Some(opts), config).await?;
    info!(
        name = %name,
        container_id = %response.id,
        "container criado com sucesso"
    );
    if !response.warnings.is_empty() {
        warn!(
            name = %name,
            warnings = ?response.warnings,
            "Docker retornou warnings"
        );
    }
    Ok(response.id)
}

pub async fn start(docker: &Docker, container_id: &str) -> Result<()> {
    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        "iniciando container"
    );

    docker
        .start_container(container_id, None::<StartContainerOptions<String>>)
        .await?;

    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        "container em execução"
    );
    Ok(())
}

/// Retorna `true` se o erro do bollard for um 404 (container inexistente).
///
/// Um `container_id`/nome desatualizado no DB (ex.: após uma promoção que
/// removeu o container antigo mas não persistiu o novo id a tempo) não deve
/// ser tratado como falha ao parar/remover — o estado desejado já é
/// satisfeito.
pub(super) fn is_not_found(e: &BollardError) -> bool {
    matches!(
        e,
        BollardError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

pub async fn stop_graceful(docker: &Docker, container_id: &str, timeout: i64) -> Result<()> {
    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        timeout = timeout,
        "parando container"
    );

    let opts = StopContainerOptions { t: timeout };
    if let Err(e) = docker.stop_container(container_id, Some(opts)).await {
        if is_not_found(&e) {
            warn!(
                container_id = %format!(
                    "...{}",
                    &container_id[..container_id.len().min(10)]
                ),
                "container já não existe, ignorando"
            );
            return Ok(());
        }
        return Err(e.into());
    }

    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        "container parado"
    );

    Ok(())
}

pub async fn rename(docker: &Docker, container_id: &str, new_name: &str) -> Result<()> {
    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        new_name = %new_name, "renomeando container"
    );

    let opts = RenameContainerOptions {
        name: new_name.to_string(),
    };

    docker.rename_container(container_id, opts).await?;
    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        new_name = %new_name, "renomeado"
    );
    Ok(())
}

pub async fn remove(docker: &Docker, container_id: &str) -> Result<()> {
    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        "removendo container"
    );

    let opts = RemoveContainerOptions {
        force: true,
        v: true,
        ..Default::default()
    };
    if let Err(e) = docker.remove_container(container_id, Some(opts)).await {
        if is_not_found(&e) {
            warn!(
                container_id = %format!(
                    "...{}",
                    &container_id[..container_id.len().min(10)]
                ),
                "container já não existe, ignorando"
            );
            return Ok(());
        }
        return Err(e.into());
    }
    info!(
    container_id = %format!(
        "...{}",
        &container_id[..container_id.len().min(10)]),
        "removido"
    );
    Ok(())
}
