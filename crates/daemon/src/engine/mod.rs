//! Cliente Docker (`DockerClient`, via bollard) e os submódulos por recurso.
//!
//! O mesmo código serve o Docker e o Podman: o Podman expõe a API compatível
//! com Docker num socket (`podman.socket`), que o bollard usa sem mudança, e o
//! CLI (`compose`, `pull`, `exec`…) é invocado pelo binário do motor — ver
//! [`Engine`] e [`cli`].

pub mod compose;
pub mod containers;
pub mod images;
pub mod inventory_cache;
pub mod networks;

use std::sync::OnceLock;

use anyhow::Result;
use bollard::Docker;
use tokio::process::Command;

/// Motor de containers por trás do socket configurado.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Docker,
    Podman,
}

impl Engine {
    /// `RUSTPLOY_CONTAINER_ENGINE=podman|docker` manda; sem ele, um socket com
    /// "podman" no caminho (`/run/podman/podman.sock`, `$XDG_RUNTIME_DIR/podman/…`)
    /// indica Podman. Evita um campo novo no `config.toml` (o parse é tudo-ou-nada).
    pub fn detect(socket_path: &str) -> Self {
        match std::env::var("RUSTPLOY_CONTAINER_ENGINE").as_deref() {
            Ok("podman") => Engine::Podman,
            Ok("docker") => Engine::Docker,
            _ if socket_path.contains("podman") => Engine::Podman,
            _ => Engine::Docker,
        }
    }

    pub fn binary(self) -> &'static str {
        match self {
            Engine::Docker => "docker",
            Engine::Podman => "podman",
        }
    }
}

static ENGINE: OnceLock<(Engine, String)> = OnceLock::new();

/// Motor ativo (Docker até o `connect` dizer o contrário).
pub fn engine() -> Engine {
    ENGINE.get().map_or(Engine::Docker, |(e, _)| *e)
}

/// `Command` do CLI do motor ativo, já apontado para o socket configurado.
/// Use no lugar de `Command::new("docker")`.
///
/// Docker: `DOCKER_HOST`. Podman: o `podman` local ignora o `DOCKER_HOST` e
/// usaria o storage do usuário que roda o daemon (rootless), não o do socket
/// — por isso `--remote --url`, que também vale para o `podman compose`
/// (o provider `docker-compose` herda o `DOCKER_HOST`).
pub fn cli() -> Command {
    let mut cmd = Command::new(engine().binary());
    if let Some((engine, socket)) = ENGINE.get() {
        if *engine == Engine::Podman {
            cmd.args(["--remote", "--url", &format!("unix://{socket}")]);
        }
        cmd.env("DOCKER_HOST", format!("unix://{socket}"));
    }
    cmd
}

pub struct DockerClient {
    pub inner: Docker,
    pub _socket_path: String,
}

impl DockerClient {
    pub fn connect(socket_path: &str) -> Result<Self> {
        let _ = ENGINE.set((Engine::detect(socket_path), socket_path.to_string()));
        let docker = Docker::connect_with_unix(socket_path, 120, bollard::API_DEFAULT_VERSION)?;
        Ok(Self {
            inner: docker,
            _socket_path: socket_path.to_string(),
        })
    }

    pub async fn ping(&self) -> Result<()> {
        self.inner.ping().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_pelo_caminho_do_socket() {
        assert_eq!(Engine::detect("/var/run/docker.sock"), Engine::Docker);
        assert_eq!(Engine::detect("/run/podman/podman.sock"), Engine::Podman);
    }
}
