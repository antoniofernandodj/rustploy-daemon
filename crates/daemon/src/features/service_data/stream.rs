//! Roda um comando do motor ligando o stdin/stdout a **arquivos por cópia explícita** (pipes), em vez
//! de herdar o descritor do arquivo: com `podman --remote` o descritor herdado não é confiável (o
//! tar do outro lado lia "short read"). É o mesmo jeito do pipe de `shared_db::exec_sh`.

use anyhow::{Result, anyhow};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Executa `cmd`. `stdin_from`: arquivo cujo conteúdo vai para o stdin; `stdout_to`: arquivo que
/// recebe o stdout. Devolve `Ok(())` se o comando saiu com 0, senão `Err(stderr)`.
pub async fn run(
    mut cmd: Command,
    stdin_from: Option<&Path>,
    stdout_to: Option<&Path>,
) -> Result<()> {
    cmd.stdin(if stdin_from.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(if stdout_to.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("falha ao executar o motor de containers: {e}"))?;

    let feeder = match (stdin_from, child.stdin.take()) {
        (Some(path), Some(mut sink)) => {
            let path = path.to_path_buf();
            Some(tokio::spawn(async move {
                let mut file = tokio::fs::File::open(path).await?;
                tokio::io::copy(&mut file, &mut sink).await?;
                sink.shutdown().await?; // EOF para o tar do outro lado
                Ok::<_, std::io::Error>(())
            }))
        }
        _ => None,
    };
    let drainer = match (stdout_to, child.stdout.take()) {
        (Some(path), Some(mut source)) => {
            let path = path.to_path_buf();
            Some(tokio::spawn(async move {
                let mut file = tokio::fs::File::create(path).await?;
                tokio::io::copy(&mut source, &mut file).await?;
                file.flush().await?;
                Ok::<_, std::io::Error>(())
            }))
        }
        _ => None,
    };
    let mut err_pipe = child.stderr.take();
    let stderr = tokio::spawn(async move {
        let mut s = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_string(&mut s).await;
        }
        s
    });

    let status = child.wait().await?;
    let stderr = stderr.await.unwrap_or_default();
    // Erro de E/S do lado do arquivo só importa se o comando deu certo (senão a causa é o stderr).
    let io_feed = match feeder {
        Some(h) => h.await.map_err(|e| anyhow!(e.to_string()))?,
        None => Ok(()),
    };
    let io_drain = match drainer {
        Some(h) => h.await.map_err(|e| anyhow!(e.to_string()))?,
        None => Ok(()),
    };
    if !status.success() {
        return Err(anyhow!("{}", stderr.trim()));
    }
    io_feed?;
    io_drain?;
    Ok(())
}
