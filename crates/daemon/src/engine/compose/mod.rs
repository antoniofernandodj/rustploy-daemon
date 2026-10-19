//! Serviços e jobs Docker Compose: `up` de stack com a rede do projeto
//! injetada, execução one-shot de job com cancelamento, e login no registry
//! embutido.

mod down;
mod jobs;
mod logs;
mod up;

pub use down::*;
pub use jobs::*;
use logs::*;
pub use up::*;

/// Última parte da saída de erro de um subprocesso, para a mensagem de falha.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let start = text.len().saturating_sub(600);
    let start = (start..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::event_bus::EventBus;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn keeps_and_strips_main_service_line() {
        assert_eq!(
            filter_main_service_line("main-1  | hello world", "main"),
            Some("hello world")
        );
    }

    #[test]
    fn drops_other_service_lines() {
        assert_eq!(
            filter_main_service_line("side-1  | hello world", "main"),
            None
        );
    }

    #[test]
    fn keeps_main_service_with_hyphen_in_name() {
        assert_eq!(
            filter_main_service_line("my-service-1  | booted", "my-service"),
            Some("booted")
        );
    }

    #[test]
    fn drops_compose_control_lines_without_pipe() {
        assert_eq!(
            filter_main_service_line(" Container rp-main-1 Started ", "main"),
            None
        );
        assert_eq!(
            filter_main_service_line("main-1 exited with code 0", "main"),
            None
        );
    }

    #[test]
    fn drops_lines_without_replica_suffix() {
        assert_eq!(filter_main_service_line("main | hello", "main"), None);
    }

    #[test]
    fn detects_prefix_of_any_service() {
        assert_eq!(
            split_service_prefix("side-1  | hello"),
            Some(("side", "hello"))
        );
        assert_eq!(split_service_prefix(" Container rp-main-1 Started "), None);
    }

    #[test]
    fn cleans_ansi_and_spinner_frames_from_compose_lines() {
        assert_eq!(
            clean_compose_line(
                "\u{1b}[1A\u{1b}[2K\u{1b}[31mError\u{1b}[0m: port is already allocated"
            ),
            Some("Error: port is already allocated".to_string())
        );
        assert_eq!(
            clean_compose_line(" ⠋ Container a  Starting\r ✔ Container a  Started  "),
            Some(" ✔ Container a  Started".to_string())
        );
        assert_eq!(clean_compose_line("\u{1b}[2K   "), None);
    }

    #[tokio::test]
    async fn keeps_only_compose_lines_in_tail() {
        let bus = EventBus::new();
        let db = crate::db::Db::connect_lazy("sqlite::memory:").unwrap();
        let tail = Mutex::new(VecDeque::new());
        let input: &[u8] =
            b"main-1  | hello\nside-1  | ignored\n\x1b[31mfailed to bind port 5432\x1b[0m\n\n";
        read_compose_output(
            input,
            shared::protocol::LogStream::Stderr,
            "main",
            &bus,
            &db,
            "job_x",
            "jrun_x",
            None,
            &tail,
        )
        .await;
        assert_eq!(
            tail.into_inner().unwrap(),
            VecDeque::from(["failed to bind port 5432".to_string()])
        );
    }

    #[tokio::test]
    async fn wait_for_cancel_returns_immediately_if_already_true() {
        // Cobre o caso "cancelamento chegou antes de alguém observar":
        // watch::Receiver rastreia versão, então mesmo enviado ANTES desta
        // chamada começar, `changed()` não perde o sinal.
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            wait_for_cancel(&mut rx),
        )
        .await
        .expect("wait_for_cancel não deveria bloquear quando já está true");
    }

    #[tokio::test]
    async fn wait_for_cancel_resolves_when_sent_later() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let waiter = tokio::spawn(async move {
            wait_for_cancel(&mut rx).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("wait_for_cancel deveria resolver após o send")
            .unwrap();
    }

    #[tokio::test]
    async fn wait_for_cancel_returns_when_sender_dropped() {
        // Sender derrubado sem nunca enviar `true` (task já sumiu) — não
        // pode travar pra sempre.
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        drop(tx);
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            wait_for_cancel(&mut rx),
        )
        .await
        .expect("wait_for_cancel não deveria travar com o sender derrubado");
    }

    #[test]
    fn compose_images_separate_pulled_images_from_built_services() {
        let config = r#"{"services":{
            "db":{"image":"postgres:18"},
            "cache":{"image":"postgres:18"},
            "app":{"image":"app:latest","build":{"context":"."}},
            "worker":{"build":{"context":"worker"}}
        }}"#;
        let images = parse_compose_images(config).unwrap();
        assert_eq!(
            images.pulled_images,
            ["postgres:18"],
            "dedup, e `build:` nunca é baixado"
        );
        assert!(images.has_build);
    }

    #[test]
    fn compose_images_without_build_services_do_not_depend_on_build_cache() {
        let images = parse_compose_images(r#"{"services":{"db":{"image":"mysql:8"}}}"#).unwrap();
        assert_eq!(images.pulled_images, ["mysql:8"]);
        assert!(!images.has_build);
    }

    #[test]
    fn compose_images_reject_garbage_output() {
        assert!(parse_compose_images("not json").is_err());
    }
}
