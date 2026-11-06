//! Streaming dos logs dos containers gerenciados para o event bus, reconectando
//! e encerrando streams parados.

use crate::{platform::event_bus::EventBus, store::Db};
use bollard::{
    Docker,
    container::{LogOutput, LogsOptions},
};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use shared::{Event, protocol::LogStream};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::interval};
use tracing::{debug, warn};

// Timeout para detectar streams travados: se não chegar nenhum dado em 30s
// e o container não estiver mais rodando, encerra o stream.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn stream_loop(docker: Arc<Docker>, db: Arc<Db>, bus: Arc<EventBus>) {
    let active: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut ticker = interval(Duration::from_secs(5));

    loop {
        ticker.tick().await;

        let services = match crate::store::services::get_running(&db).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "logs: failed to get running services");
                continue;
            }
        };

        let new_containers: Vec<(String, String)> = {
            let locked = active.lock().await;
            services
                .into_iter()
                .filter_map(|svc| {
                    let cid = svc.live_container_id?;
                    if locked.contains(&cid) {
                        return None;
                    }
                    Some((cid, svc.id))
                })
                .collect()
        };

        for (container_id, service_id) in new_containers {
            active.lock().await.insert(container_id.clone());

            let docker2 = docker.clone();
            let bus2 = bus.clone();
            let active2 = active.clone();
            let cid = container_id.clone();

            tokio::spawn(async move {
                stream_container(docker2, cid.clone(), service_id, bus2).await;
                active2.lock().await.remove(&cid);
            });
        }
    }
}

/// Transmite os logs de um container para o event bus até o stream acabar, ou
/// até ficar parado por `STREAM_IDLE_TIMEOUT` com o container já fora do ar.
async fn stream_container(
    docker: Arc<Docker>,
    container_id: String,
    service_id: String,
    bus: Arc<EventBus>,
) {
    // Com o histórico persistido: retoma a partir da última linha gravada (o escritor ainda descarta
    // repetidas); sem histórico, as últimas 100 como sempre. `timestamps` dá o instante REAL de cada
    // linha, que é o que se grava (e que torna a deduplicação exata).
    let resume_from = last_persisted(&service_id, &container_id).await;
    register_exec(&docker, &service_id, &container_id).await;
    let opts = LogsOptions::<String> {
        follow: true,
        stdout: true,
        stderr: true,
        timestamps: true,
        since: resume_from.map_or(0, |t| t.timestamp()),
        tail: if resume_from.is_some() {
            "all".into()
        } else {
            "100".into()
        },
        ..Default::default()
    };

    let mut stream = docker.logs(&container_id, Some(opts));
    let mut stamper = Stamper::default();

    loop {
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await {
            // Linha normal
            Ok(Some(Ok(LogOutput::StdOut { message }))) => {
                publish_log(
                    &bus,
                    &mut stamper,
                    &service_id,
                    &container_id,
                    false,
                    &message,
                );
            }
            Ok(Some(Ok(LogOutput::StdErr { message }))) => {
                publish_log(
                    &bus,
                    &mut stamper,
                    &service_id,
                    &container_id,
                    true,
                    &message,
                );
            }
            Ok(Some(Ok(_))) => continue,

            // Erro no stream (container parou, etc.)
            Ok(Some(Err(e))) => {
                debug!(error = %e, container = %container_id, "logs: stream encerrado com erro");
                break;
            }

            // Stream encerrou normalmente
            Ok(None) => {
                debug!(container = %container_id, "logs: stream encerrado");
                break;
            }

            // Timeout: nenhum dado em STREAM_IDLE_TIMEOUT segundos.
            // Verifica se o container ainda está rodando.
            Err(_) => {
                let running = docker
                    .inspect_container(
                        &container_id,
                        None::<bollard::container::InspectContainerOptions>,
                    )
                    .await
                    .ok()
                    .and_then(|info| info.state?.running)
                    .unwrap_or(false);

                if !running {
                    debug!(container = %container_id, "logs: container parou, encerrando stream travado");
                    break;
                }
                // Container ainda rodando mas silencioso — continua esperando
            }
        }
    }
}

/// Anota no armazenamento a que deploy e a que imagem pertence a execução (um `inspect_container`,
/// feito uma vez por stream). Falhar aqui só deixa a lista sem essas colunas: nunca interrompe o log.
async fn register_exec(docker: &Docker, service_id: &str, container_id: &str) {
    let Some(store) = crate::platform::log_store::global() else {
        return;
    };
    let Ok(info) = docker
        .inspect_container(
            container_id,
            None::<bollard::container::InspectContainerOptions>,
        )
        .await
    else {
        return;
    };
    let config = info.config;
    let deployment_id = config
        .as_ref()
        .and_then(|c| c.labels.as_ref())
        .and_then(|l| l.get("rustploy.deployment_id"))
        .cloned();
    let image = config.and_then(|c| c.image);
    store.set_exec(service_id, container_id, deployment_id, image);
}

/// Último instante já gravado desse container (se o armazenamento está ligado).
async fn last_persisted(service_id: &str, container_id: &str) -> Option<DateTime<Utc>> {
    let store = crate::platform::log_store::global()?;
    let (sid, cid) = (service_id.to_string(), container_id.to_string());
    tokio::task::spawn_blocking(move || store.last_timestamp(&sid, &cid))
        .await
        .ok()
        .flatten()
}

/// Dá a cada linha de um stream um instante **único e determinístico**. O Podman entrega os timestamps
/// do log com resolução de segundo, então várias linhas chegam com o mesmo instante; a deduplicação do
/// armazenamento (`log_store`: descarta o que não é mais novo que o último gravado) as confundiria.
/// A n-ésima linha que repete o instante recebe `+ n` nanossegundos. Como a retomada de um stream (e a
/// captura final) sempre recomeça no início do segundo (`since` é em segundos), a repetição gera
/// exatamente os mesmos instantes e é reconhecida como já gravada.
#[derive(Default)]
struct Stamper {
    prev: Option<DateTime<Utc>>,
    repeats: i64,
}

impl Stamper {
    fn stamp(&mut self, real: DateTime<Utc>) -> DateTime<Utc> {
        if self.prev == Some(real) {
            self.repeats += 1;
        } else {
            self.prev = Some(real);
            self.repeats = 0;
        }
        real + chrono::Duration::nanoseconds(self.repeats)
    }
}

/// Separa o prefixo de instante que o Docker põe com `timestamps: true` (`2026-…Z texto`).
/// Sem prefixo válido, usa agora e devolve o texto inteiro.
fn split_timestamp(raw: &str) -> (DateTime<Utc>, &str) {
    if let Some((head, rest)) = raw.split_once(' ')
        && let Ok(at) = DateTime::parse_from_rfc3339(head)
    {
        return (at.with_timezone(&Utc), rest);
    }
    (Utc::now(), raw)
}

/// Uma saída do Docker pode trazer várias linhas: cada uma é gravada e publicada separadamente.
fn publish_log(
    bus: &EventBus,
    stamper: &mut Stamper,
    service_id: &str,
    container_id: &str,
    is_stderr: bool,
    bytes: &[u8],
) {
    let raw = String::from_utf8_lossy(bytes);
    for line in raw.split('\n') {
        let (at, text) = split_timestamp(line.trim_end_matches('\r'));
        if text.is_empty() {
            continue;
        }
        let at = stamper.stamp(at);
        if let Some(store) = crate::platform::log_store::global() {
            store.append(service_id, container_id, is_stderr, at, text);
        }
        bus.publish(Event::LogLine {
            service_id: service_id.to_string(),
            container_id: container_id.to_string(),
            stream: if is_stderr {
                LogStream::Stderr
            } else {
                LogStream::Stdout
            },
            line: text.to_string(),
            timestamp: at,
        });
    }
}

/// Lê o que um container ainda tem e o grava, **antes** de ele ser removido (troca no deploy,
/// rollback de staging). O stream contínuo só segue o container já `live`, então sem isto o log de
/// um container de staging que cai em segundos, ou o fim do log do antigo, se perderia.
/// Sem armazenamento ligado, não faz nada. Nunca falha o chamador.
pub async fn capture_final(docker: &Docker, service_id: &str, container_id: &str) {
    let Some(store) = crate::platform::log_store::global() else {
        return;
    };
    let since = last_persisted(service_id, container_id).await;
    register_exec(docker, service_id, container_id).await;
    let opts = LogsOptions::<String> {
        follow: false,
        stdout: true,
        stderr: true,
        timestamps: true,
        since: since.map_or(0, |t| t.timestamp()),
        tail: "all".into(),
        ..Default::default()
    };
    let mut stream = docker.logs(container_id, Some(opts));
    let mut stamper = Stamper::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next()).await;
        let (is_err, message) = match next {
            Ok(Some(Ok(LogOutput::StdOut { message }))) => (false, message),
            Ok(Some(Ok(LogOutput::StdErr { message }))) => (true, message),
            Ok(Some(Ok(_))) => continue,
            // fim do log, erro (container já sumiu) ou prazo: para por aqui
            _ => break,
        };
        for line in String::from_utf8_lossy(&message).split('\n') {
            let (at, text) = split_timestamp(line.trim_end_matches('\r'));
            if !text.is_empty() {
                let at = stamper.stamp(at);
                store.append(service_id, container_id, is_err, at, text);
            }
        }
    }
    let store2 = store.clone();
    let _ = tokio::task::spawn_blocking(move || store2.flush()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn stamper_numera_linhas_que_dividem_o_mesmo_segundo() {
        let t1 = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let t2 = Utc.timestamp_opt(1_800_000_001, 0).unwrap();
        let mut s = Stamper::default();
        let got: Vec<_> = [t1, t1, t1, t2, t2, t1]
            .into_iter()
            .map(|t| s.stamp(t).timestamp_subsec_nanos())
            .collect();
        assert_eq!(got, [0, 1, 2, 0, 1, 0]);
        // repetição (novo stream) gera exatamente os mesmos instantes
        let mut again = Stamper::default();
        assert_eq!(again.stamp(t1).timestamp_subsec_nanos(), 0);
        assert_eq!(again.stamp(t1).timestamp_subsec_nanos(), 1);
    }

    #[test]
    fn separa_o_prefixo_de_instante_do_docker() {
        let (at, texto) = split_timestamp("2026-11-05T03:43:14.913865516Z linha com espaço");
        assert_eq!(texto, "linha com espaço");
        assert_eq!(at.timestamp_subsec_nanos(), 913_865_516);
        let (_, sem_prefixo) = split_timestamp("sem timestamp aqui");
        assert_eq!(sem_prefixo, "sem timestamp aqui");
    }
}
