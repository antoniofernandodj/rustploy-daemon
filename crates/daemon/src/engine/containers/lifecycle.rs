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
/// existente. A reconexão **pede o mesmo IPv4** que o container tinha: com IP novo, toda
/// conexão que o app abriu no staging (RabbitMQ, banco, Redis…) ficava muda — o IP de origem
/// mudou — e só morria por timeout de heartbeat 2–3 min depois (visto no Chiquitos; reproduzido
/// com podman + RabbitMQ). Se o engine recusar o IP fixo, reconecta sem ele (o deploy não pode
/// falhar por isso) e o IP muda como antes — quem guardou o IP (ingress) deve relê-lo de
/// qualquer forma. Não faz nada se o alias já estiver lá (retomada de deploy pela recovery).
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
    let ipv4 = info
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
        .and_then(|n| n.get(network))
        .and_then(|e| e.ip_address.clone())
        .filter(|ip| !ip.is_empty());
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
    let connect = |ip: Option<String>| {
        docker.connect_network(
            network,
            ConnectNetworkOptions {
                container: container_id.to_string(),
                endpoint_config: endpoint_with_alias(alias, ip),
            },
        )
    };
    if let Err(e) = connect(ipv4.clone()).await {
        let Some(ip) = &ipv4 else {
            return Err(anyhow!(
                "falha ao reconectar {container_id} a {network} com alias {alias}: {e}"
            ));
        };
        warn!(container_id = %container_id, ip = %ip, error = %e, "engine recusou manter o IP; reconectando com IP novo");
        connect(None).await.map_err(|e| {
            anyhow!("falha ao reconectar {container_id} a {network} com alias {alias}: {e}")
        })?;
    }
    info!(container_id = %container_id, network = %network, alias = %alias, "alias de rede atribuído");
    Ok(())
}

/// Endpoint de rede com o alias e, se houver, o IPv4 fixo.
fn endpoint_with_alias(alias: &str, ipv4: Option<String>) -> EndpointSettings {
    EndpointSettings {
        aliases: Some(vec![alias.to_string()]),
        ipam_config: ipv4.map(|ip| bollard::models::EndpointIpamConfig {
            ipv4_address: Some(ip),
            ..Default::default()
        }),
        ..Default::default()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_leva_o_alias_e_o_ip_fixo_quando_ha() {
        let e = endpoint_with_alias("rp_api", Some("10.89.2.8".into()));
        assert_eq!(e.aliases, Some(vec!["rp_api".to_string()]));
        assert_eq!(
            e.ipam_config.and_then(|i| i.ipv4_address).as_deref(),
            Some("10.89.2.8")
        );
        let sem_ip = endpoint_with_alias("rp_api", None);
        assert!(sem_ip.ipam_config.is_none());
    }

    /// Contra um engine de verdade (Docker ou Podman): o alias entra e o **IP não muda**.
    /// `cargo test -p rustploy alias_mantem_o_ip -- --ignored --nocapture`
    /// (imagem local qualquer em `RP_TEST_IMAGE`, padrão `redis:alpine`).
    #[tokio::test]
    #[ignore = "precisa de um engine Docker/Podman rodando"]
    async fn alias_mantem_o_ip() {
        use bollard::network::CreateNetworkOptions;
        let docker = Docker::connect_with_local_defaults().expect("engine");
        let image = std::env::var("RP_TEST_IMAGE").unwrap_or_else(|_| "redis:alpine".into());
        let pid = std::process::id();
        let (net, name) = (
            format!("rp_teste_ip_net_{pid}"),
            format!("rp_teste_ip_{pid}"),
        );
        docker
            .create_network(CreateNetworkOptions {
                name: net.clone(),
                ..Default::default()
            })
            .await
            .expect("rede");
        // O 2º container mantém a rede (e o netns rootless) de pé quando o 1º desconecta;
        // sem ele, o Podman rootless tenta matar o processo do netns e leva "permission denied".
        let companheiro = format!("{name}_c");
        for n in [&name, &companheiro] {
            docker
                .create_container(
                    Some(CreateContainerOptions {
                        name: n.clone(),
                        platform: None,
                    }),
                    Config {
                        image: Some(image.clone()),
                        host_config: Some(HostConfig {
                            network_mode: Some(net.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await
                .expect("container");
            docker
                .start_container(n, None::<StartContainerOptions<String>>)
                .await
                .expect("start");
        }

        let ip_de = |docker: Docker, name: String, net: String| async move {
            let i = docker
                .inspect_container(&name, None::<InspectContainerOptions>)
                .await
                .unwrap();
            let ep = i.network_settings.unwrap().networks.unwrap()[&net].clone();
            (
                ep.ip_address.unwrap_or_default(),
                ep.aliases.unwrap_or_default(),
            )
        };
        let (antes, _) = ip_de(docker.clone(), name.clone(), net.clone()).await;
        let r = attach_network_alias(&docker, &name, &net, "rp_teste_alias").await;
        let (depois, aliases) = ip_de(docker.clone(), name.clone(), net.clone()).await;

        for n in [&name, &companheiro] {
            let _ = docker
                .remove_container(
                    n,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
        }
        let _ = docker.remove_network(&net).await;

        r.expect("attach_network_alias");
        assert!(!antes.is_empty());
        assert_eq!(antes, depois, "o IP mudou na reconexão");
        assert!(aliases.iter().any(|a| a == "rp_teste_alias"), "{aliases:?}");
    }
}
