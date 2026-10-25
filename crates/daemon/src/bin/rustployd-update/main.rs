//! `rustployd-update` — o helper root que instala pacotes **assinados** do rustploy.
//!
//! O daemon (`rustployd`) roda sem privilégios e não consegue se atualizar. Este binário
//! roda como root, ativado pelo systemd via socket (`rustployd-update.socket` →
//! `/run/rustploy/update.sock`, `root:rustploy 0660`), e tem superfície mínima por construção:
//!
//! - **não abre conexão de rede**: quem baixa é o daemon; aqui só se confere e instala;
//! - só dois verbos (`install` e `rollback`, ver `protocol.rs`);
//! - só instala o pacote `rustployd`, só o que estiver assinado por uma chave de confiança
//!   embutida, e nunca uma versão que não seja mais nova que a instalada;
//! - nenhuma string do cliente chega a um shell (argv direto, sem `sh -c`).
//!
//! Atende **um** pedido por ativação e sai: o systemd o sobe de novo no próximo.
//! Desenho e motivos: `docs/plano-updaters-daemon-e-gui.md`, seção 4.

mod protocol;

use protocol::{Request, Response};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::fd::FromRawFd,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    time::Duration,
};
use update_core::{
    installer::{Paths, Settings, Updater},
    signature::TRUSTED_PUBLIC_KEYS,
    system::{RealSystem, System},
};

const ALLOWED_DIR: &str = "/var/lib/rustploy/updates";
const STATE_DIR: &str = "/var/lib/rustploy-update";
const MAX_DEB_BYTES: u64 = 200 * 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 4096;

/// Socket herdado do systemd (LISTEN_FDS, fd 3) ou, em desenvolvimento, criado em
/// `$RUSTPLOY_UPDATE_SOCKET`. O segundo modo também libera trocar os diretórios
/// (`RUSTPLOY_UPDATE_ALLOWED_DIR`, `RUSTPLOY_UPDATE_STATE_DIR`): sob o systemd o ambiente
/// é fixo e nada disso vale, então um ambiente alterado não redireciona um helper root.
fn listener() -> (UnixListener, bool) {
    if std::env::var("LISTEN_FDS").ok().as_deref() == Some("1") {
        // SAFETY: contrato do sd_listen_fds — com LISTEN_FDS=1 o primeiro (e único) socket
        // herdado é sempre o fd 3, aberto e em modo listen.
        return (unsafe { UnixListener::from_raw_fd(3) }, false);
    }
    let path = std::env::var("RUSTPLOY_UPDATE_SOCKET").unwrap_or_else(|error| {
        eprintln!(
            "rustployd-update: sem LISTEN_FDS do systemd nem RUSTPLOY_UPDATE_SOCKET ({error})"
        );
        std::process::exit(1);
    });
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap_or_else(|error| {
        eprintln!("rustployd-update: falha ao criar o socket {path}: {error}");
        std::process::exit(1);
    });
    (listener, true)
}

/// As chaves em que este helper confia: as de produção embutidas em `update-core`.
///
/// Exceção só para o teste de ponta a ponta (`crates/daemon/tests/e2e-updater`): se
/// `RUSTPLOY_E2E_TRUSTED_KEY` estiver definida **na hora de compilar**, ela vale no lugar da de
/// produção, para o teste assinar pacotes de mentira com uma chave descartável. É em tempo de
/// compilação de propósito: um ambiente alterado em execução nunca troca as chaves de um helper
/// root. O CI e o `make deb` não definem a variável, e um teste confere que o build normal usa
/// as chaves de produção.
fn trusted_keys() -> Vec<String> {
    match option_env!("RUSTPLOY_E2E_TRUSTED_KEY") {
        Some(key) if !key.is_empty() => vec![key.to_string()],
        _ => TRUSTED_PUBLIC_KEYS
            .iter()
            .map(|key| key.to_string())
            .collect(),
    }
}

fn directory(variable: &str, default: &str, development: bool) -> PathBuf {
    match std::env::var(variable) {
        Ok(path) if development => PathBuf::from(path),
        _ => PathBuf::from(default),
    }
}

fn main() {
    let (listener, development) = listener();
    let paths = Paths {
        allowed_dir: directory("RUSTPLOY_UPDATE_ALLOWED_DIR", ALLOWED_DIR, development),
        state_dir: directory("RUSTPLOY_UPDATE_STATE_DIR", STATE_DIR, development),
        managed_binaries: vec![
            PathBuf::from("/usr/bin/rustployd"),
            PathBuf::from("/usr/libexec/rustployd-fw"),
            PathBuf::from("/usr/libexec/rustployd-update"),
        ],
    };
    let settings = Settings {
        package: "rustployd".into(),
        service: Some("rustployd".into()),
        trusted_keys: trusted_keys(),
        max_deb_bytes: MAX_DEB_BYTES,
        poll_interval: Duration::from_secs(1),
        stable_polls: 15,
        max_polls: 90,
    };
    let system = RealSystem;
    let updater = Updater {
        paths: &paths,
        settings: &settings,
        system: &system,
    };

    eprintln!("rustployd-update: pronto (atende um pedido e sai)");
    match listener.accept() {
        Ok((stream, _)) => serve(stream, &updater),
        Err(error) => eprintln!("rustployd-update: accept: {error}"),
    }
}

fn serve<S: System>(stream: UnixStream, updater: &Updater<'_, S>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(mut writer) = stream.try_clone() else {
        eprintln!("rustployd-update: não consegui clonar a conexão");
        return;
    };
    // O limite de bytes impede que um cliente mande uma "linha" sem fim.
    let mut line = String::new();
    if BufReader::new(stream.take(MAX_REQUEST_BYTES))
        .read_line(&mut line)
        .is_err()
    {
        let _ = writer.write_all(
            Response::failed("pedido ilegível ou lento demais")
                .to_line()
                .as_bytes(),
        );
        return;
    }
    handle_line(updater, &line, &mut |response| {
        let _ = writer.write_all(response.to_line().as_bytes());
        let _ = writer.flush();
    });
}

/// Um pedido. Para `install` a resposta "aceito" sai **antes** de instalar: o `dpkg` reinicia o
/// daemon, que é o cliente desta conexão, e o resultado final vai para o `last.json`.
fn handle_line<S: System>(updater: &Updater<'_, S>, line: &str, reply: &mut dyn FnMut(Response)) {
    let request = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => request,
        Err(error) => {
            reply(Response::failed(format!("pedido inválido: {error}")));
            return;
        }
    };
    match request {
        Request::Install { deb, sig } => match updater.prepare(&deb, &sig) {
            Ok(prepared) => {
                eprintln!(
                    "rustployd-update: aceito {}, instalando",
                    prepared.facts.version
                );
                reply(Response::accepted(Some(prepared.facts.version.clone())));
                let outcome = updater.execute(prepared);
                eprintln!("rustployd-update: {:?}: {}", outcome.kind, outcome.message);
            }
            Err(message) => {
                eprintln!("rustployd-update: recusado: {message}");
                reply(Response::failed(message));
            }
        },
        Request::Rollback => match updater.prepare_rollback() {
            Ok(plan) => {
                eprintln!(
                    "rustployd-update: revertendo para {:?}",
                    plan.previous_version
                );
                reply(Response::accepted(Some(plan.previous_version.clone())));
                let outcome = updater.execute_rollback(plan);
                eprintln!("rustployd-update: {:?}: {}", outcome.kind, outcome.message);
            }
            Err(message) => reply(Response::failed(message)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, io, time::Duration};
    use update_core::system::CommandOutput;

    /// Sem sistema nenhum: qualquer comando é um erro (estes testes não devem chegar a rodar um).
    struct Nothing;
    impl System for Nothing {
        fn run(
            &self,
            program: &str,
            _args: &[&str],
            _envs: &[(&str, &str)],
        ) -> io::Result<CommandOutput> {
            Err(io::Error::other(format!("não devia rodar {program}")))
        }
        fn sleep(&self, _duration: Duration) {}
    }

    fn with_updater<R>(test: impl FnOnce(&Updater<'_, Nothing>) -> R) -> R {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            allowed_dir: root.path().join("updates"),
            state_dir: root.path().join("state"),
            managed_binaries: vec![],
        };
        std::fs::create_dir_all(&paths.allowed_dir).unwrap();
        let settings = Settings {
            package: "rustployd".into(),
            service: Some("rustployd".into()),
            trusted_keys: vec![],
            max_deb_bytes: 1024,
            poll_interval: Duration::ZERO,
            stable_polls: 1,
            max_polls: 1,
        };
        test(&Updater {
            paths: &paths,
            settings: &settings,
            system: &Nothing,
        })
    }

    fn replies(line: &str) -> Vec<Response> {
        with_updater(|updater| {
            let collected = RefCell::new(vec![]);
            handle_line(updater, line, &mut |response| {
                collected.borrow_mut().push(response)
            });
            collected.into_inner()
        })
    }

    /// Guarda contra um build de teste que vaze para o pacote: sem a variável, só valem as chaves
    /// de produção. (Se este teste falhar, alguém compilou com `RUSTPLOY_E2E_TRUSTED_KEY` definida.)
    #[test]
    fn a_normal_build_trusts_only_the_production_keys() {
        assert!(
            option_env!("RUSTPLOY_E2E_TRUSTED_KEY").is_none_or(str::is_empty),
            "RUSTPLOY_E2E_TRUSTED_KEY está definida neste build: não é um build de produção"
        );
        let expected: Vec<String> = TRUSTED_PUBLIC_KEYS
            .iter()
            .map(|key| key.to_string())
            .collect();
        assert_eq!(trusted_keys(), expected);
    }

    #[test]
    fn garbage_and_unknown_verbs_get_one_failed_reply() {
        for line in [
            "",
            "isto não é json",
            r#"{"op":"exec","cmd":"id"}"#,
            r#"{"op":"install"}"#,
        ] {
            let replies = replies(line);
            assert_eq!(replies.len(), 1, "{line:?}");
            assert!(!replies[0].ok, "{line:?}");
        }
    }

    #[test]
    fn a_path_outside_the_updates_dir_is_refused_in_one_reply() {
        let replies = replies(r#"{"op":"install","deb":"/etc/passwd","sig":"/etc/shadow"}"#);
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].ok);
        assert!(
            replies[0]
                .error
                .as_deref()
                .unwrap()
                .contains("pacote recusado")
        );
    }

    #[test]
    fn rollback_without_a_backup_is_refused() {
        let replies = replies(r#"{"op":"rollback"}"#);
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].ok);
    }
}
