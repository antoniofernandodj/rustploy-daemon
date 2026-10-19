//! Leitura e limpeza da saída do compose, linha a linha, para o log do job/deploy.

use crate::platform::{event_bus::EventBus, log_sink::LogSink};
use chrono::Utc;
use shared::Event;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Quantas linhas do próprio `docker compose` (build, pull, erros de
/// container) o job guarda pra despejar no log quando falha.
pub(super) const COMPOSE_TAIL_MAX: usize = 200;

/// Separa o prefixo `<service>-<replica> |` que o `docker compose up` (sem
/// `-d`) põe em cada linha de saída de container. Devolve o nome do serviço
/// e o resto da linha; `None` pra linha sem esse prefixo — as linhas de
/// controle do próprio compose (`Container x Started`, build, spinners).
pub(super) fn split_service_prefix(line: &str) -> Option<(&str, &str)> {
    let (prefix, rest) = line.split_once('|')?;
    let prefix = prefix.trim();
    let (name, replica) = prefix.rsplit_once('-')?;
    if replica.is_empty() || !replica.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((name, rest.trim_start_matches(' ')))
}

/// Filtra uma linha bruta de `docker compose up` (sem `-d`) pra só sobrar a
/// saída do `main_service`: sem `-d`, o compose intercala stdout/stderr de
/// TODOS os serviços do stack (prefixo `<service>-<replica> | <msg>`, ex.:
/// `main-1  | hello`) e ainda emite linhas de controle próprias (`Container
/// x Started`, spinners com códigos ANSI) sem esse prefixo — tudo isso
/// deixa "Ver logs" do job ilegível quando o compose tem mais de um serviço
/// (ex.: um serviço gatilho + um banco auxiliar). Só a linha de conteúdo do
/// `main_service` sobrevive (prefixo removido); linhas de outros serviços e
/// linhas de controle do compose (sem esse prefixo) são descartadas.
pub(super) fn filter_main_service_line<'a>(line: &'a str, main_service: &str) -> Option<&'a str> {
    match split_service_prefix(line)? {
        (name, rest) if name == main_service => Some(rest),
        _ => None,
    }
}

/// Limpa uma linha de controle do compose pra caber no log do job: fica só
/// o último quadro de um spinner (texto depois do último `\r`) e sem os
/// códigos ANSI de cor/cursor. `None` quando não sobra texto.
pub(super) fn clean_compose_line(line: &str) -> Option<String> {
    let line = line.rsplit('\r').next().unwrap_or(line);
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI (`ESC [ ... <final>`): pula até o byte final (0x40..=0x7E).
        // Outros escapes de dois caracteres (`ESC x`): pula só o `x`.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
    }
    let out = out.trim_end();
    (!out.trim().is_empty()).then(|| out.to_string())
}

/// Grava uma linha no log do job (tabela `job_log` + evento ao vivo) e, se o
/// job espelha um deployment, no build log dele também.
pub(super) async fn record_job_line(
    bus: &EventBus,
    db: &impl LogSink,
    job_id: &str,
    job_run_id: &str,
    mirror: Option<&(String, String)>,
    stream: shared::protocol::LogStream,
    line: String,
) {
    let ts = Utc::now();
    bus.publish(Event::JobLogLine {
        job_run_id: job_run_id.to_string(),
        job_id: job_id.to_string(),
        line: line.clone(),
        timestamp: ts,
        stream: stream.clone(),
    });
    let _ = db.append_job_line(job_run_id, &stream, &line, ts).await;
    if let Some((deployment_id, service_id)) = mirror {
        let _ = db.append_build_line(deployment_id, &line, ts).await;
        bus.publish(Event::BuildLog {
            deployment_id: deployment_id.clone(),
            service_id: service_id.clone(),
            line,
            timestamp: ts,
        });
    }
}

/// Lê um dos pipes do `docker compose up` até fechar. A saída do
/// `main_service` vai direto pro log do job; a de outros serviços é
/// descartada; as linhas do próprio compose vão pro `compose_tail`, que só
/// é despejado no log se o job falhar — é onde mora a causa quando o stack
/// nem chega a rodar o `main_service` (build quebrado, porta ocupada…).
#[allow(clippy::too_many_arguments)]
pub(super) async fn read_compose_output<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    stream: shared::protocol::LogStream,
    main_service: &str,
    bus: &EventBus,
    db: &impl LogSink,
    job_id: &str,
    job_run_id: &str,
    mirror: Option<&(String, String)>,
    compose_tail: &Mutex<VecDeque<String>>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(raw_line)) = lines.next_line().await {
        if let Some(line) = filter_main_service_line(&raw_line, main_service) {
            if !line.trim().is_empty() {
                let line = line.to_string();
                record_job_line(bus, db, job_id, job_run_id, mirror, stream.clone(), line).await;
            }
            continue;
        }
        if split_service_prefix(&raw_line).is_some() {
            continue;
        }
        if let Some(line) = clean_compose_line(&raw_line) {
            let mut tail = compose_tail.lock().unwrap_or_else(|e| e.into_inner());
            if tail.len() == COMPOSE_TAIL_MAX {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
}
