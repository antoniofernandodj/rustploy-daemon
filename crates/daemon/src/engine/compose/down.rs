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
    // Compose vazio (serviço criado e nunca preenchido): não há stack para
    // derrubar, e `compose -f - down` recusa o arquivo vazio ("empty compose
    // file"). Sem isto o serviço nunca saía de `Stopping` nem podia ser removido.
    if content.trim().is_empty() {
        info!(project = %project_name, "compose_down: compose vazio, nada a derrubar");
        return Ok(());
    }
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
        let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
        return Err(anyhow!("docker compose down falhou: {}", stderr.trim()));
    }
    info!(project = %project_name, "compose_down: concluído");
    Ok(())
}

/// Tira as sequências de cor (`ESC [ … m`) que o compose escreve no stderr, para a
/// mensagem ficar legível num toast/UI em vez de `␛[4m>>>>`.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strip_ansi_remove_cores() {
        assert_eq!(strip_ansi("\u{1b}[4m>>>>\u{1b}[0m empty"), ">>>> empty");
    }
}
