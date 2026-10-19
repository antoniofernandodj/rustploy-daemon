//! `compose down` de uma stack.

use anyhow::{Result, anyhow};
use tokio::io::AsyncWriteExt;
use tracing::info;

pub async fn down(
    content: &str,
    project_name: &str,
    _network_name: &str,
    env_vars: &[(String, String)],
) -> Result<()> {
    info!(project = %project_name, "compose_down: iniciando");
    let mut child = crate::engine::cli()
        .args([
            "compose",
            "-p",
            project_name,
            "-f",
            "-",
            "down",
            "--remove-orphans",
        ])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("falha ao iniciar docker compose down: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content.as_bytes()).await;
        stdin.shutdown().await.ok();
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| anyhow!("falha ao aguardar compose down: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker compose down falhou: {}", stderr.trim()));
    }
    info!(project = %project_name, "compose_down: concluído");
    Ok(())
}
