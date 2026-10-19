//! Inspeção de um container: IP, logs e metadados.

use anyhow::{Result, anyhow};
use bollard::{
    Docker,
    container::{InspectContainerOptions, LogsOptions},
};
use futures::StreamExt;
use tracing::{debug, info};

pub async fn inspect(
    docker: &Docker,
    container_id: &str,
) -> Result<bollard::models::ContainerInspectResponse> {
    debug!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        "inspecionando"
    );

    let resp = docker
        .inspect_container(container_id, None::<InspectContainerOptions>)
        .await?;
    let running = resp.state.as_ref().and_then(|s| s.running).unwrap_or(false);
    let status = resp.state.as_ref().and_then(|s| s.status);
    debug!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        running = running,
        status = ?status,
        "resultado"
    );
    Ok(resp)
}

/// IP do container na rede dada, lido via `docker network inspect` (o IP do
/// `container inspect` vem vazio em algumas combinações de Docker/bollard).
pub async fn get_container_ip(
    docker: &Docker,
    container_id: &str,
    network_name: &str,
) -> Result<String> {
    // Usa `docker network inspect` (NetworkContainer.ipv4_address) em vez de
    // `docker container inspect` (EndpointSettings.ip_address).
    // EndpointSettings.ip_address vem vazio em alguns Docker/bollard combos;
    // NetworkContainer.ipv4_address é uma struct diferente e mais confiável.
    debug!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        network = %network_name,
        "inspecionando rede"
    );

    let net_info = docker
        .inspect_network::<String>(network_name, None)
        .await
        .map_err(|e| anyhow!("falha ao inspecionar rede {network_name}: {e}"))?;

    let net_containers = net_info.containers.unwrap_or_default();

    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        network = %network_name,
        count = net_containers.len(),
        ids = ?net_containers
            .keys()
            .map(|k| &k[..k.len().min(12)])
            .collect::<Vec<_>>(),
        "containers encontrados na rede"
    );

    // Chave do mapa é o container ID completo (64 hex chars)
    let nc = net_containers
        .get(container_id)
        .or_else(|| {
            net_containers
                .iter()
                .find(|(k, _)| k.starts_with(container_id) || container_id.starts_with(k.as_str()))
                .map(|(_, v)| v)
        })
        .ok_or_else(|| {
            let ids: Vec<String> = net_containers
                .keys()
                .map(|k| k[..k.len().min(12)].to_string())
                .collect();
            anyhow!("container não encontrado na rede {network_name} (presentes: {ids:?})")
        })?;

    info!(
        container_id = %format!("...{}", &container_id[..container_id.len().min(10)]),
        network = %network_name,
        ipv4 = ?nc.ipv4_address,
        mac = ?nc.mac_address,
        "NetworkContainer encontrado"
    );

    // ipv4_address vem no formato CIDR "172.18.0.2/16" — extrai só o IP
    let ip = nc
        .ipv4_address
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.split('/').next().unwrap_or(s).to_string())
        .ok_or_else(|| anyhow!("sem IPv4 para container na rede {network_name}"))?;

    info!(
        container_id = %format!(
            "...{}",
            &container_id[..container_id.len().min(10)]
        ),
        network = %network_name,
        ip = %ip, "IP resolvido"
    );
    Ok(ip)
}

/// Returns the last `tail` lines of stdout+stderr from a container (best-effort).
pub async fn get_container_logs(docker: &Docker, container_id: &str, tail: usize) -> Vec<String> {
    let opts = LogsOptions::<String> {
        stdout: true,
        stderr: true,
        tail: tail.to_string(),
        ..Default::default()
    };
    let mut stream = docker.logs(container_id, Some(opts));
    let mut lines = Vec::new();
    while let Some(Ok(output)) = stream.next().await {
        let text = output.to_string();
        for line in text.lines() {
            if !line.is_empty() {
                lines.push(line.to_string());
            }
        }
    }
    lines
}
