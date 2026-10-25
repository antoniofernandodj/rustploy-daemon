//! HTTP/JSON + SSE control API — the daemon's remote administrative channel.
//!
//! Serves the same `Command`→`Response` surface the local UDS server does, but
//! over HTTP so the glacier-ui client can drive it entirely from Luau
//! (`fetch`/`sse`). It reuses [`dispatch`] wholesale — only the transport
//! changes. By default it binds loopback and serves plain HTTP, meant to sit
//! behind the ingress proxy. Alternatively, setting `api.domain` in the config
//! makes this listener terminate TLS on its own port with an ACME-provisioned
//! certificate, so the GUI can reach it directly over HTTPS.
//!
//! - `POST /api/rpc` — body is a JSON-encoded [`Command`]; the reply is a
//!   JSON-encoded [`Response`](shared::Response). One endpoint covers every
//!   command; the client reshapes the (externally tagged) JSON in Luau.
//! - `GET /api/events` — Server-Sent Events. Emits a `snapshot` record with the
//!   full dashboard state every 2s (replacing the old client-side 2s poll) plus
//!   a `bus` record for each live [`Event`](shared::Event) from the event bus
//!   (logs, metrics, deploy progress). One connection replaces poll + stream.
//!   Each record is **self-describing**: its JSON `data` carries a `kind` field
//!   (`"snapshot"`/`"bus"`) because the SSE client (glacier-ui) discards the
//!   `event:` line and only sees `data:`, so the discriminator must live inside
//!   the payload.
//! - `GET /api/health` — liveness probe (`ok`).

pub mod dashboard;
pub mod public_routes;

use std::convert::Infallible;
use std::sync::Arc;

use crate::platform::config::ApiConfig;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use shared::{Command, Event, Response as RpResponse};
use tracing::{error, info, warn};

use super::agent;
use super::web_ui;
use crate::api::dispatch::dispatch;
use crate::bootstrap::state::AppState;
use crate::features::ingress::TlsManager;

/// Unified response body: both the buffered (`Full`) replies and the streaming
/// (`StreamBody`) SSE body box to this, so a single `service_fn` can return
/// either. Error is `Infallible` since neither body ever fails to produce data.
type ApiBody = BoxBody<Bytes, Infallible>;

/// Starts the API listener. Returns on bind failure; otherwise loops forever.
/// Intended to be `tokio::spawn`ed from `main`.
///
/// When `cfg.domain` is set (non-empty), the listener terminates TLS **on this
/// same port** using a Let's Encrypt certificate provisioned through `tls`
/// (auto-renewed by the ingress' ACME loop). Otherwise it serves plain HTTP,
/// as before. The port is always taken from `cfg.port` (config.toml `[api].port`).
pub async fn run(state: AppState, cfg: ApiConfig, tls: Option<Arc<TlsManager>>) {
    // Trim: um token vindo de `EnvironmentFile` ou de um TOML copiado à mão
    // chega com frequência com espaço/`\n` em volta. O header HTTP já perde
    // esses bytes na borda (OWS), então SEM o trim aqui o cliente manda o token
    // certo e leva 401 — com a diferença invisível dos dois lados. Feito ANTES
    // do guard de bind público: um token só de espaços tem que contar como
    // ausente ali também, senão o guard passa e a porta abre sem auth.
    if let Some(t) = cfg.token.as_deref()
        && t != t.trim()
    {
        warn!(
            "API: api.token tem espaço/quebra de linha em volta — ignorados \
                 na comparação, mas vale limpar a config"
        );
    }
    let token = cfg
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Safety guard: refuse a non-loopback bind without a token.
    if cfg.is_public_bind() && token.is_none() {
        warn!(
            bind = %cfg.bind_address,
            "API: bind não-loopback sem token configurado — listener NÃO iniciado. \
             Defina api.token (ou RUSTPLOY_API_TOKEN) para expor remotamente."
        );
        return;
    }

    let addr = format!("{}:{}", cfg.bind_address, cfg.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(
                error = %e,
                addr = %addr,
                "API: falha ao bind, listener desabilitado"
            );
            return;
        }
    };

    // TLS opcional: se `api.domain` estiver definido, a própria porta da API
    // termina HTTPS com cert automático via ACME. Provisiona/valida o cert
    // ANTES de aceitar conexões (o desafio HTTP-01 exige a porta 80 acessível).
    let acceptor: Option<tokio_rustls::TlsAcceptor> =
        match cfg.domain.as_deref().filter(|d| !d.is_empty()) {
            Some(domain) => {
                let Some(tls) = tls else {
                    error!(
                        domain,
                        "API: api.domain configurado mas TlsManager indisponível — \
                         listener NÃO iniciado"
                    );
                    return;
                };
                if let Err(e) = tls.ensure_cert(domain).await {
                    warn!(
                        domain, error = %e,
                        "API: falha ao provisionar certificado TLS — o handshake HTTPS \
                         falhará até o cert existir (confira ACME habilitado + porta 80)"
                    );
                }
                info!(domain, "API HTTP/SSE: TLS ativo (cert automático via ACME)");
                Some(tls.tls_acceptor())
            }
            None => None,
        };

    let token = Arc::new(token);
    info!(
        addr = %addr,
        auth = token.is_some(),
        tls = acceptor.is_some(),
        "API HTTP/SSE: escutando"
    );

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "API: accept falhou");
                continue;
            }
        };
        let state = state.clone();
        let token = token.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            match acceptor {
                Some(acc) => match acc.accept(stream).await {
                    Ok(tls_stream) => {
                        serve_conn(TokioIo::new(tls_stream), state, token, peer).await
                    }
                    Err(e) => tracing::debug!(
                        peer = %peer, error = %e, "API: TLS handshake falhou"
                    ),
                },
                None => serve_conn(TokioIo::new(stream), state, token, peer).await,
            }
        });
    }
}

/// Serves one HTTP/1.1 connection over `io` (plain TCP or a TLS stream).
async fn serve_conn<I>(
    io: I,
    state: AppState,
    token: Arc<Option<String>>,
    peer: std::net::SocketAddr,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + 'static,
{
    let svc = service_fn(move |req| handle(req, state.clone(), token.clone(), peer));
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, svc)
        .await
    {
        // Long-lived SSE connections end with an error when the client
        // drops — expected, so keep it at debug.
        tracing::debug!(peer = %peer, error = %e, "API: conexão encerrada");
    }
}

/// Roteia uma requisição HTTP da API: rotas públicas (webhook, callback OAuth)
/// antes do gate do token Bearer, e depois `/api/rpc`, o SSE `/api/events`, o
/// upload de zip e os arquivos da webui.
async fn handle(
    req: Request<Incoming>,
    state: AppState,
    token: Arc<Option<String>>,
    peer: std::net::SocketAddr,
) -> Result<Response<ApiBody>, Infallible> {
    // Rotas públicas — servidas ANTES do gate de Bearer porque cada uma tem a
    // sua própria autenticação (token de 192 bits na URL, no webhook; `state`
    // CSRF, no callback OAuth). São chamadas por terceiros (GitHub/Gitea/Docker
    // Hub, o navegador no fim do fluxo OAuth) que não têm o token da API.
    // Fora do `match` abaixo (não só `if let`) porque `webhook()` precisa
    // consumir `req` inteiro (corpo incluso, pro filtro de branch) — um braço
    // de match que empresta `req.uri().path()` no padrão não libera o
    // empréstimo a tempo do `move` implícito no `.await` seguinte.
    if req.uri().path().starts_with("/webhook/") {
        return Ok(boxed(public_routes::webhook(req, state).await));
    }
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/oauth/gitea/callback" | "/oauth/github/callback") => {
            return Ok(boxed(public_routes::oauth_callback(req, state).await));
        }
        // Versão em texto puro, sem token: serve para um `curl` descobrir qual
        // binário está no ar (a versão sozinha não distingue dois builds do
        // mesmo número, por isso o commit junto).
        (&Method::GET, "/api/version") => {
            return Ok(text(
                StatusCode::OK,
                format!(
                    "rustployd {} ({})\n",
                    env!("CARGO_PKG_VERSION"),
                    env!("RUSTPLOY_GIT_SHA")
                ),
            ));
        }
        // Login por token de conexão — público por natureza (é ele que entrega o
        // token). Ver `agent::login` e docs/plano-chave-de-acesso-e-mcp.md.
        (&Method::POST, "/api/login") => {
            // Com `[api].domain` o listener termina TLS (ver `ApiConfig::domain`).
            let scheme = if state
                .api
                .domain
                .as_deref()
                .is_some_and(|d| !d.trim().is_empty())
            {
                "https"
            } else {
                "http"
            };
            return Ok(login(req, token.as_deref(), peer, scheme).await);
        }
        (&Method::GET, p) => {
            // Web UI/PWA (crates/daemon/webui/): HTML/CSS/JS/ícones
            // estáticos, sem dado nenhum — servidos fora do gate de token de
            // propósito (ver módulo `web_ui`, docstring). `None` quando `p`
            // não é uma rota do app shell, caindo no restante do match.
            if let Some(resp) = web_ui::serve(p) {
                return Ok(resp);
            }
        }
        _ => {}
    }

    // Bearer-token auth (constant-time), only when a token is configured.
    if let Some(expected) = token.as_ref()
        && let Err(motivo) = auth_check(&req, expected)
    {
        warn!(
            peer = %peer,
            path = %req.uri().path(),
            motivo = %motivo,
            "API: 401 — requisição recusada"
        );
        return Ok(text(StatusCode::UNAUTHORIZED, "unauthorized"));
    }

    // Upload de zip: `POST /api/services/<id>/archive`. Tratado antes do `match`
    // porque o handler **move** `req` (lê o corpo binário), o que colide com o
    // borrow que o discriminante do `match` mantém sobre `req.uri().path()`.
    if req.method() == Method::POST {
        let path = req.uri().path().to_owned();
        if let Some(id) = path
            .strip_prefix("/api/services/")
            .and_then(|s| s.strip_suffix("/archive"))
        {
            return Ok(if id.is_empty() {
                text(StatusCode::NOT_FOUND, "not found")
            } else {
                service_archive_upload(req, state, id.to_string()).await
            });
        }
    }

    Ok(match (req.method(), req.uri().path()) {
        (&Method::POST, "/api/rpc") => rpc(req, state).await,
        (&Method::POST, "/mcp") => mcp(req, state).await,
        // Sem stream SSE servidor→cliente no MCP: o spec manda 405 no GET.
        (&Method::GET | &Method::DELETE, "/mcp") => {
            text(StatusCode::METHOD_NOT_ALLOWED, "use POST /mcp")
        }
        (&Method::GET, "/api/agent/manual") => Response::builder()
            .header(hyper::header::CONTENT_TYPE, "text/markdown; charset=utf-8")
            .body(Full::new(Bytes::from_static(agent::MANUAL.as_bytes())).boxed())
            .unwrap(),
        (&Method::GET, "/api/access-key") => {
            let chave = agent::access_key_for(
                &agent::public_address(&state.api, &crate::platform::config::outbound_ip()),
                token.as_deref(),
            );
            json_value(StatusCode::OK, &serde_json::json!({ "key": chave }))
        }
        (&Method::GET, "/api/events") => events(state),
        (&Method::GET, "/api/health") => text(StatusCode::OK, "ok"),
        // SSE dedicado de logs de runtime de um serviço: `/api/services/<id>/logs`.
        (&Method::GET, p) if p.starts_with("/api/services/") && p.ends_with("/logs") => {
            // strip_prefix + strip_suffix (não aritmética de índices) para o caso
            // sem id (`/api/services/logs`) cair fora em vez de fatiar inválido.
            let id = p
                .strip_prefix("/api/services/")
                .and_then(|s| s.strip_suffix("/logs"))
                .unwrap_or("");
            if id.is_empty() {
                text(StatusCode::NOT_FOUND, "not found")
            } else {
                service_logs(state, id.to_string())
            }
        }
        // SSE dedicado de build logs de um deployment: `/api/deployments/<id>/build-logs`.
        (&Method::GET, p) if p.starts_with("/api/deployments/") && p.ends_with("/build-logs") => {
            let id = p
                .strip_prefix("/api/deployments/")
                .and_then(|s| s.strip_suffix("/build-logs"))
                .unwrap_or("");
            if id.is_empty() {
                text(StatusCode::NOT_FOUND, "not found")
            } else {
                deployment_build_logs(state, id.to_string())
            }
        }
        // SSE dedicado de logs de UMA execução de job: `/api/jobs/runs/<id>/logs`.
        (&Method::GET, p) if p.starts_with("/api/jobs/runs/") && p.ends_with("/logs") => {
            let id = p
                .strip_prefix("/api/jobs/runs/")
                .and_then(|s| s.strip_suffix("/logs"))
                .unwrap_or("");
            if id.is_empty() {
                text(StatusCode::NOT_FOUND, "not found")
            } else {
                job_run_logs(state, id.to_string())
            }
        }
        _ => text(StatusCode::NOT_FOUND, "not found"),
    })
}

/// `POST /api/login`: troca o token de conexão pelo token da API (ver `agent::login`).
async fn login(
    req: Request<Incoming>,
    expected: Option<&str>,
    peer: std::net::SocketAddr,
    scheme: &str,
) -> Response<ApiBody> {
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return text(StatusCode::BAD_REQUEST, "erro ao ler o corpo"),
    };
    let (status, resposta) = agent::login(&body, expected, host.as_deref(), scheme);
    if status == StatusCode::UNAUTHORIZED {
        warn!(peer = %peer, "API: login com chave de outra instalação — recusado");
    }
    json_value(status, &resposta)
}

/// `POST /mcp`: o servidor MCP do agente (ver `agent::mcp`).
async fn mcp(req: Request<Incoming>, state: AppState) -> Response<ApiBody> {
    // A qual servidor o agente está ligado (ver `agent::mcp::handle`).
    let server = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            agent::public_address(&state.api, &crate::platform::config::outbound_ip())
        });
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return text(StatusCode::BAD_REQUEST, "erro ao ler o corpo"),
    };
    match agent::mcp::handle(&state, &body, &server).await {
        Some((status, resposta)) => json_value(status, &resposta),
        None => Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Full::new(Bytes::new()).boxed())
            .unwrap(),
    }
}

fn json_value(status: StatusCode, body: &serde_json::Value) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header(
            hyper::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )
        .body(
            Full::new(Bytes::from(
                serde_json::to_vec_pretty(body).unwrap_or_default(),
            ))
            .boxed(),
        )
        .unwrap()
}

async fn service_archive_upload(
    req: Request<Incoming>,
    state: AppState,
    service_id: String,
) -> Response<ApiBody> {
    let accept_gzip = accepts_gzip(&req);
    let filename = req
        .headers()
        .get("x-rustploy-filename")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return text(StatusCode::BAD_REQUEST, "erro ao ler o corpo"),
    };
    if body.len() > super::handlers::services::service_archive_upload::MAX_ZIP_BYTES {
        return text(StatusCode::PAYLOAD_TOO_LARGE, "zip excede 100 MiB");
    }
    let resp = super::handlers::services::service_archive_upload::handle(
        state, service_id, body, filename,
    )
    .await;
    match serde_json::to_vec(&resp) {
        Ok(bytes) => json_response(bytes, accept_gzip),
        Err(e) => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("erro ao serializar resposta: {e}"),
        ),
    }
}

/// `POST /api/rpc`: decode a `Command`, run it through `dispatch`, encode the
/// `Response` back as JSON.
async fn rpc(req: Request<Incoming>, state: AppState) -> Response<ApiBody> {
    // Lê o Accept-Encoding ANTES de consumir o corpo com `into_body`.
    let accept_gzip = accepts_gzip(&req);
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return text(StatusCode::BAD_REQUEST, "erro ao ler o corpo"),
    };
    let cmd: Command = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => return text(StatusCode::BAD_REQUEST, format!("comando inválido: {e}")),
    };
    let resp = dispatch(state, cmd).await;
    match serde_json::to_vec(&resp) {
        Ok(bytes) => json_response(bytes, accept_gzip),
        Err(e) => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("erro ao serializar resposta: {e}"),
        ),
    }
}

/// `GET /api/events`: SSE stream. A producer task feeds an mpsc channel with the
/// periodic snapshot and every live bus event; the response body streams those
/// frames out until the client disconnects (which closes the channel).
fn events(state: AppState) -> Response<ApiBody> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        let mut bus_rx = state.bus.subscribe();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Snapshot inicial imediato, para o cliente pintar sem esperar o 1º tick.
        if tx
            .send(sse_frame("snapshot", &snapshot(&state).await))
            .await
            .is_err()
        {
            return;
        }

        // Firehose do dashboard: NÃO carrega logs de container (`LogLine`) nem de
        // build (`BuildLog`). Um serviço verborrágico (logando por request) emite
        // centenas de linhas/s; entregá-las aqui forçaria a janela PRINCIPAL a
        // reavaliar sua árvore inteira por linha (todas as janelas do iced::daemon
        // dividem UMA thread de UI), congelando tudo. Logs vão por endpoints
        // dedicados, consumidos só pelas janelas de log ISOLADAS (motores leves):
        //   • runtime → `/api/services/{id}/logs`         (ver `service_logs`)
        //   • build   → `/api/deployments/{id}/build-logs` (ver `deployment_build_logs`)
        // Aqui ficam só snapshot + eventos não-log (métricas, deploy, status),
        // todos de baixa frequência e enviados imediatamente.
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if tx.send(
                        sse_frame("snapshot", &snapshot(&state).await)
                    ).await.is_err() {
                        break;
                    }
                }
                r = bus_rx.recv() => match r {
                    Ok(ev) => {
                        // Logs (runtime/build/job) só pelos endpoints dedicados.
                        if matches!(
                            ev,
                            Event::LogLine { .. } | Event::BuildLog { .. } | Event::JobLogLine { .. }
                        ) {
                            continue;
                        }
                        // Self-describing: the SSE client only sees `data:` (the
                        // `event:` line is dropped), so tag the payload with
                        // `kind:"bus"` and nest the event under `event`.
                        let wrapped = serde_json::json!({ "kind": "bus", "event": ev });
                        if let Ok(js) = serde_json::to_string(&wrapped)
                            && tx.send(sse_frame("bus", &js)).await.is_err() {
                                break;
                            }
                    }
                    // Slow consumer dropped events: skip and carry on.
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "SSE de eventos: eventos descartados do canal (consumidor lento)");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    });

    sse_response(rx)
}

/// `GET /api/services/{id}/logs`: SSE dedicado aos logs de container de UM
/// serviço. Existe para tirar o firehose de logs da janela principal (ver
/// `events`): um produtor filtra `Event::LogLine` do serviço no bus e os
/// entrega coalescidos em lotes `bus_batch` (mesma framing de `send_log_batch`),
/// consumidos pela janela de logs (motor Glacier leve). Não manda snapshot nem o
/// tail — a janela semeia o histórico inicial via `open_window({data})`.
fn service_logs(state: AppState, service_id: String) -> Response<ApiBody> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        let mut bus_rx = state.bus.subscribe();
        const LOG_BATCH_MAX: usize = 400;
        let mut batch: Vec<Event> = Vec::new();
        let mut batch_flush = tokio::time::interval(std::time::Duration::from_millis(80));
        batch_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = batch_flush.tick() => {
                    if !batch.is_empty()
                        && send_log_batch(&tx, std::mem::take(&mut batch)).await.is_err()
                    {
                        break;
                    }
                }
                r = bus_rx.recv() => match r {
                    Ok(ev) => {
                        let keep = matches!(
                            &ev,
                            Event::LogLine { service_id: sid, .. } if *sid == service_id
                        );
                        if keep {
                            batch.push(ev);
                            if batch.len() >= LOG_BATCH_MAX
                                && send_log_batch(&tx, std::mem::take(&mut batch)).await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // Logs de container não ficam no banco: não há como repor, só avisar.
                        tracing::warn!(service_id = %service_id, skipped, "SSE de logs do serviço: eventos descartados do canal (consumidor lento)");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    });

    sse_response(rx)
}

/// `GET /api/deployments/{id}/build-logs`: SSE dedicado à saída de `docker build`
/// de UM deployment. Gêmeo de `service_logs`, mas filtrando `Event::BuildLog` por
/// `deployment_id`; existe para tirar os build logs do firehose (ver `events`) e
/// os isolar na janela de build logs (motor leve). Não manda o histórico — a
/// janela o semeia via `GetBuildLogs` em `open_window({data})`.
fn deployment_build_logs(state: AppState, deployment_id: String) -> Response<ApiBody> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        let connected_at = chrono::Utc::now();
        let mut bus_rx = state.bus.subscribe();
        let mut seen = SeenLines::new();
        let mut service_id = String::new();
        const LOG_BATCH_MAX: usize = 400;
        let mut batch: Vec<Event> = Vec::new();
        let mut batch_flush = tokio::time::interval(std::time::Duration::from_millis(80));
        batch_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = batch_flush.tick() => {
                    if !batch.is_empty()
                        && send_log_batch(&tx, std::mem::take(&mut batch)).await.is_err()
                    {
                        break;
                    }
                }
                r = bus_rx.recv() => match r {
                    Ok(ev) => {
                        let keep = matches!(
                            &ev,
                            Event::BuildLog { deployment_id: did, .. } if *did == deployment_id
                        );
                        if keep {
                            if let Event::BuildLog { timestamp, line, service_id: sid, .. } = &ev {
                                service_id.clone_from(sid);
                                if !mark_seen(&mut seen, *timestamp, line) {
                                    continue;
                                }
                            }
                            batch.push(ev);
                            if batch.len() >= LOG_BATCH_MAX
                                && send_log_batch(&tx, std::mem::take(&mut batch)).await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(deployment_id = %deployment_id, skipped, "SSE de build logs: eventos descartados do canal, recuperando do banco");
                        if let Ok(db_lines) = crate::store::build_logs::get_for_deployment(&state.db, &deployment_id).await {
                            for l in recover_lines(db_lines, connected_at, &mut seen) {
                                batch.push(Event::BuildLog {
                                    deployment_id: deployment_id.clone(),
                                    service_id: service_id.clone(),
                                    line: l.line,
                                    timestamp: l.timestamp,
                                });
                            }
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    });

    sse_response(rx)
}

/// `GET /api/jobs/runs/{id}/logs`: SSE dedicado à saída de UMA execução de job
/// one-shot. Gêmeo de `deployment_build_logs`, mas filtrando `Event::JobLogLine`
/// por `job_run_id`; não manda o histórico — a janela o semeia via
/// `GetJobLogs` em `open_window({data})`.
fn job_run_logs(state: AppState, job_run_id: String) -> Response<ApiBody> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        let connected_at = chrono::Utc::now();
        let mut bus_rx = state.bus.subscribe();
        let mut seen = SeenLines::new();
        let mut job_id = String::new();
        const LOG_BATCH_MAX: usize = 400;
        let mut batch: Vec<Event> = Vec::new();
        let mut batch_flush = tokio::time::interval(std::time::Duration::from_millis(80));
        batch_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = batch_flush.tick() => {
                    if !batch.is_empty()
                        && send_log_batch(&tx, std::mem::take(&mut batch)).await.is_err()
                    {
                        break;
                    }
                }
                r = bus_rx.recv() => match r {
                    Ok(ev) => {
                        let keep = matches!(
                            &ev,
                            Event::JobLogLine { job_run_id: rid, .. } if *rid == job_run_id
                        );
                        if keep {
                            if let Event::JobLogLine { timestamp, line, job_id: jid, .. } = &ev {
                                job_id.clone_from(jid);
                                if !mark_seen(&mut seen, *timestamp, line) {
                                    continue;
                                }
                            }
                            batch.push(ev);
                            if batch.len() >= LOG_BATCH_MAX
                                && send_log_batch(&tx, std::mem::take(&mut batch)).await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // O canal descartou eventos (consumidor lento): o banco tem tudo,
                        // então repõe o que faltou em vez de deixar buracos no log.
                        tracing::warn!(job_run_id = %job_run_id, skipped, "SSE de logs do job: eventos descartados do canal, recuperando do banco");
                        if let Ok(db_lines) = crate::store::job_log::get_for_run(&state.db, &job_run_id).await {
                            for l in recover_lines(db_lines, connected_at, &mut seen) {
                                batch.push(Event::JobLogLine {
                                    job_run_id: job_run_id.clone(),
                                    job_id: job_id.clone(),
                                    line: l.line,
                                    timestamp: l.timestamp,
                                    stream: l.stream,
                                });
                            }
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    });

    sse_response(rx)
}

/// Teto do conjunto de linhas já entregues por um stream de logs (a deduplicação
/// da recuperação). Estourou, zera: o pior caso é repetir linhas numa recuperação
/// depois de um log com dezenas de milhares de linhas.
const SEEN_LINES_MAX: usize = 50_000;

type SeenLines = std::collections::HashSet<(chrono::DateTime<chrono::Utc>, String)>;

/// Marca uma linha como entregue. `false` se já tinha sido (duplicata vinda do
/// bus depois de a mesma linha ter sido recuperada do banco).
fn mark_seen(seen: &mut SeenLines, ts: chrono::DateTime<chrono::Utc>, line: &str) -> bool {
    if seen.len() >= SEEN_LINES_MAX {
        seen.clear();
    }
    seen.insert((ts, line.to_string()))
}

/// As linhas do banco que o consumidor perdeu quando o canal descartou eventos
/// (`RecvError::Lagged`): as de `ts >= since` (o stream só cobre o que veio depois
/// de conectar; o histórico anterior é do seed da janela) que ainda não foram
/// entregues. Na ordem do banco, já marcadas como vistas.
fn recover_lines(
    db_lines: Vec<shared::protocol::BuildLogLine>,
    since: chrono::DateTime<chrono::Utc>,
    seen: &mut SeenLines,
) -> Vec<shared::protocol::BuildLogLine> {
    db_lines
        .into_iter()
        .filter(|l| l.timestamp >= since)
        .filter(|l| mark_seen(seen, l.timestamp, &l.line))
        .collect()
}

/// Intervalo do comentário de "sinal de vida" nos SSE. Um stream que só emite
/// quando há dado (logs de um job com build silencioso de minutos, por exemplo)
/// fica ocioso, e NAT/firewall/proxy no caminho derrubam conexão ociosa sem
/// avisar o cliente: as linhas seguintes nunca chegam e a janela parece "parada".
const SSE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);

/// Response body comum dos endpoints SSE (`events`/`service_logs`/
/// `deployment_build_logs`/`job_run_logs`): drena o `rx` para o corpo
/// `text/event-stream`, com buffering de proxy desligado p/ os eventos saírem na
/// hora através do ingress. A cada `SSE_KEEPALIVE` sem tráfego manda um
/// comentário SSE (`: keepalive`), que os clientes ignoram, para a conexão nunca
/// ficar ociosa. Quando o cliente some, o envio falha e a tarefa solta o `rx`
/// (o produtor percebe o canal fechado e encerra, como antes).
fn sse_response(mut rx: tokio::sync::mpsc::Receiver<Bytes>) -> Response<ApiBody> {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    tokio::spawn(async move {
        let mut beat = tokio::time::interval(SSE_KEEPALIVE);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        beat.tick().await; // o primeiro tick é imediato; pula
        loop {
            let chunk = tokio::select! {
                msg = rx.recv() => match msg {
                    Some(bytes) => bytes,
                    None => break,
                },
                _ = beat.tick() => Bytes::from_static(b": keepalive\n\n"),
            };
            if out_tx.send(chunk).await.is_err() {
                break;
            }
        }
    });
    let stream = futures::stream::poll_fn(move |cx| {
        out_rx
            .poll_recv(cx)
            .map(|opt| opt.map(|b| Ok::<_, Infallible>(Frame::data(b))))
    });

    let body = StreamBody::new(stream).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        // Disable proxy buffering so events are flushed promptly through ingress.
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}

/// Builds the full dashboard snapshot as one JSON object, reusing `dispatch`
/// for each piece — the same bundle the old client-side 2s poll fetched. The
/// client (Luau) reshapes/filters it. `services` fans out one `ServiceList` per
/// project, tagging each service with its project name.
pub(crate) async fn snapshot(state: &AppState) -> String {
    use serde_json::{Map, Value, json};
    let mut obj = Map::new();
    // Self-describing record (see module docs): the SSE client only sees the
    // `data:` payload, so the `snapshot`/`bus` discriminator lives here.
    obj.insert("kind".into(), Value::String("snapshot".into()));

    if let RpResponse::DaemonStatus(d) = dispatch(state.clone(), Command::DaemonStatus).await {
        obj.insert(
            "status".into(),
            serde_json::to_value(d).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::DeploymentSummaries(list) =
        dispatch(state.clone(), Command::RecentDeployments { limit: 40 }).await
    {
        obj.insert(
            "deployments".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::Projects(projects) = dispatch(state.clone(), Command::ProjectList).await {
        // Uma única listagem do Docker indexa todos os containers por service_id
        // e por projeto Compose, para anexar a cada serviço os containers reais
        // em execução (id + nome + estado) — cobre réplicas, staging e Compose.
        let container_index =
            crate::engine::containers::index_containers(&state.docker.inner).await;
        let mut services = Vec::new();
        let mut projects_json = Vec::new();
        for p in &projects {
            // Só os NOMES dos secrets do projeto (o blob cifrado nunca sai do
            // daemon). Anexados ao objeto do projeto como `containers` é
            // anexado ao do serviço logo abaixo — é dado só do snapshot, então
            // não vira campo do model `Project` de `shared`.
            let mut pv = serde_json::to_value(p).unwrap_or(Value::Null);
            if let RpResponse::SecretNames(names) = dispatch(
                state.clone(),
                Command::SecretList {
                    project_id: p.id.clone(),
                },
            )
            .await
                && let Value::Object(m) = &mut pv
            {
                m.insert(
                    "secrets".into(),
                    serde_json::to_value(names).unwrap_or(Value::Null),
                );
            }
            projects_json.push(pv);

            if let RpResponse::Services(svcs) = dispatch(
                state.clone(),
                Command::ServiceList {
                    project_id: p.id.clone(),
                },
            )
            .await
            {
                for s in svcs {
                    let conts = container_index.for_service(&s);
                    let mut sv = serde_json::to_value(&s).unwrap_or(Value::Null);
                    if let Value::Object(m) = &mut sv {
                        m.insert(
                            "containers".into(),
                            serde_json::to_value(conts).unwrap_or(Value::Null),
                        );
                    }
                    services.push(json!({ "project_name": p.name, "service": sv }));
                }
            }
        }

        obj.insert("projects".into(), Value::Array(projects_json));

        obj.insert("services".into(), Value::Array(services));
    }
    // Painel "Em execução": projetos e serviços rodando, já classificados.
    obj.insert(
        "dashboard".into(),
        serde_json::to_value(dashboard::load(state).await).unwrap_or(Value::Null),
    );
    // Selo "aquecido/frio" de cada serviço com build (Git/zip); as telas só exibem.
    if let RpResponse::ServiceWarmStatuses(list) =
        dispatch(state.clone(), Command::ServiceWarmStatusList).await
    {
        obj.insert(
            "service_warm".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::DockerImages(list) = dispatch(state.clone(), Command::DockerImages).await {
        obj.insert(
            "docker_images".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::DockerVolumes(list) = dispatch(state.clone(), Command::DockerVolumes).await {
        obj.insert(
            "docker_volumes".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::DockerNetworks(list) = dispatch(state.clone(), Command::DockerNetworks).await
    {
        obj.insert(
            "docker_networks".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::DockerContainers(list) =
        dispatch(state.clone(), Command::DockerContainers).await
    {
        obj.insert(
            "docker_containers".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::JobSummaries(list) = dispatch(state.clone(), Command::JobListAll).await {
        obj.insert(
            "jobs".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::DeployEngineStatus(eng) =
        dispatch(state.clone(), Command::DeployEngineStatus).await
    {
        obj.insert(
            "engine".into(),
            serde_json::to_value(eng).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::RegistryStatus(info) = dispatch(state.clone(), Command::RegistryStatus).await
    {
        obj.insert(
            "registry_status".into(),
            serde_json::to_value(info).unwrap_or(Value::Null),
        );
    }
    if let RpResponse::RegistryRepos(list) =
        dispatch(state.clone(), Command::RegistryRepoList).await
    {
        obj.insert(
            "registry_repos".into(),
            serde_json::to_value(list).unwrap_or(Value::Null),
        );
    }

    Value::Object(obj).to_string()
}

/// Formats one SSE record. `data` is single-line JSON (`serde_json::to_string`
/// never emits raw newlines), so a single `data:` line is safe.
fn sse_frame(event: &str, data: &str) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// Envia um lote coalescido de eventos de log como uma única frame SSE
/// `bus_batch` (ver o produtor em [`events`]): `{ "kind":"bus_batch",
/// "events":[<Event>,…] }`. O cliente (Luau `on_state`) itera `events` chamando
/// `apply_bus_event` para cada um. `Err(())` só quando o canal fechou (cliente saiu),
/// sinalizando o produtor a encerrar; um lote que falha ao serializar é
/// descartado sem derrubar o stream.
async fn send_log_batch(
    tx: &tokio::sync::mpsc::Sender<Bytes>,
    batch: Vec<Event>,
) -> Result<(), ()> {
    let wrapped = serde_json::json!({ "kind": "bus_batch", "events": batch });
    match serde_json::to_string(&wrapped) {
        Ok(js) => tx.send(sse_frame("bus", &js)).await.map_err(|_| ()),
        Err(_) => Ok(()),
    }
}

/// Checks the `Authorization: Bearer <token>` header against `expected`.
///
/// Devolve o MOTIVO da recusa em vez de um bool: um 401 mudo é indistinguível
/// de "token certo, daemon errado" do lado do cliente, e sem isso a única saída
/// é adivinhar. O motivo é logado (nunca o token em si — só o comprimento, que
/// já separa "colou um token truncado/com lixo" de "token de outra instalação").
fn auth_check<B>(req: &Request<B>, expected: &str) -> Result<(), String> {
    let Some(raw) = req.headers().get(hyper::header::AUTHORIZATION) else {
        return Err("sem header Authorization".into());
    };
    let Ok(raw) = raw.to_str() else {
        return Err("header Authorization não é ASCII válido".into());
    };
    // O prefixo `Bearer ` é opcional (ambos os clientes mandam), mas um esquema
    // DIFERENTE (Basic…) é erro de cliente, não token errado — vale distinguir.
    let got = match raw.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest,
        Some((scheme, _)) => {
            return Err(format!("esquema de autorização inesperado: {scheme}"));
        }
        None => raw,
    };
    if constant_time_eq(got.as_bytes(), expected.as_bytes()) {
        return Ok(());
    }
    if got.len() != expected.len() {
        return Err(format!(
            "token não confere (recebido {} bytes, esperado {} bytes)",
            got.len(),
            expected.len()
        ));
    }
    Err("token não confere (mesmo comprimento, conteúdo diferente)".into())
}

/// As rotas públicas (`public_routes`) devolvem `Full<Bytes>`; o listener
/// unificado fala `ApiBody`. `Full` é infalível, então o rebox é direto.
fn boxed(resp: Response<Full<Bytes>>) -> Response<ApiBody> {
    resp.map(|body| body.boxed())
}

fn text(status: StatusCode, body: impl Into<Bytes>) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(body.into()).boxed())
        .unwrap()
}

fn json_ok(bytes: Vec<u8>) -> Response<ApiBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(bytes)).boxed())
        .unwrap()
}

/// Corpo mínimo (bytes) para valer a pena comprimir: abaixo disto o overhead do
/// cabeçalho gzip + o custo de CPU não compensam o ganho irrisório.
const GZIP_MIN: usize = 1024;

/// O cliente aceita gzip? (`Accept-Encoding` contém "gzip"). O `fetch` do
/// glacier-ui manda esse header por padrão desde a 0.51.
fn accepts_gzip(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("gzip"))
}

/// Resposta JSON, comprimida com gzip quando o cliente aceita e o corpo passa de
/// [`GZIP_MIN`]. JSON comprime muito bem; numa conexão remota (GUI → daemon numa
/// VPS) isso corta a maior parte do tráfego — o cliente descomprime pelo
/// `Content-Encoding`. Em localhost o ganho some perto do loopback, mas o custo é
/// baixo, então comprimimos mesmo assim em vez de detectar o par. Falha ao
/// comprimir (rara) → cai no corpo sem comprimir.
fn json_response(bytes: Vec<u8>, accept_gzip: bool) -> Response<ApiBody> {
    if accept_gzip
        && bytes.len() >= GZIP_MIN
        && let Ok(gz) = gzip(&bytes)
    {
        return Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .header(hyper::header::CONTENT_ENCODING, "gzip")
            .body(Full::new(Bytes::from(gz)).boxed())
            .unwrap();
    }
    json_ok(bytes)
}

/// Comprime `data` em gzip (flate2, mesmo crate do tar do build Docker).
fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

/// Short-circuits on length mismatch but is otherwise constant-time over the
/// compared bytes. Adequate for a static admin token.
pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_com_auth(valor: Option<&str>) -> Request<()> {
        let mut b = Request::builder().uri("/api/rpc");
        if let Some(v) = valor {
            b = b.header(hyper::header::AUTHORIZATION, v);
        }
        b.body(()).unwrap()
    }

    /// `AppState` de teste (sem Docker nem rede de verdade), só para exercitar o SSE.
    async fn estado_de_teste() -> AppState {
        let tmp = std::env::temp_dir().join(format!("rustploy-sse-test-{}", ulid::Ulid::new()));
        let config = crate::platform::config::RustployConfig::default();
        let db = std::sync::Arc::new(crate::store::connect(&tmp).await.unwrap());
        let secrets = std::sync::Arc::new(
            crate::platform::secrets::SecretsManager::new(&tmp.join("master.key"), db.clone())
                .unwrap(),
        );
        let tls = std::sync::Arc::new(
            crate::features::ingress::TlsManager::new(
                tmp.join("certs"),
                config.ingress.acme.clone(),
            )
            .unwrap(),
        );
        AppState::new(
            db,
            std::sync::Arc::new(crate::engine::DockerClient::for_tests()),
            std::sync::Arc::new(crate::features::ingress::IngressController::new()),
            std::sync::Arc::new(crate::platform::event_bus::EventBus::new()),
            secrets,
            tls,
            tmp.join("db"),
            tmp.join("backup"),
            30,
            config.api,
            &config.update,
            None,
            None,
        )
    }

    /// As linhas que saem do SSE de logs de um job, na ordem em que chegaram.
    async fn linhas_do_sse(
        mut body: ApiBody,
        esperadas: usize,
        limite: std::time::Duration,
    ) -> Vec<String> {
        use http_body_util::BodyExt;
        let mut texto = String::new();
        let mut linhas: Vec<String> = Vec::new();
        let fim = tokio::time::Instant::now() + limite;
        while linhas.len() < esperadas {
            let Ok(Some(Ok(frame))) = tokio::time::timeout_at(fim, body.frame()).await else {
                break;
            };
            let Some(chunk) = frame.data_ref() else {
                continue;
            };
            texto.push_str(&String::from_utf8_lossy(chunk));
            while let Some(pos) = texto.find("\n\n") {
                let bruto: String = texto.drain(..pos + 2).collect();
                let Some(dados) = bruto.lines().find_map(|l| l.strip_prefix("data: ")) else {
                    continue;
                };
                let v: serde_json::Value = serde_json::from_str(dados).unwrap();
                for ev in v["events"].as_array().cloned().unwrap_or_default() {
                    if let Some(l) = ev["JobLogLine"]["line"].as_str() {
                        linhas.push(l.to_string());
                    }
                }
            }
        }
        linhas
    }

    /// Uma rajada de linhas (o `git clone` de um job emite ~400 em ~0,3 s) tem que
    /// sair INTEIRA, e em ordem, pelo SSE dedicado do run: perder uma linha solta
    /// aqui deixaria a janela de logs com buracos mesmo com o banco completo.
    #[tokio::test]
    async fn sse_de_job_entrega_rajada_completa_e_em_ordem() {
        use std::time::Duration;
        for espacamento_us in [0u64, 300] {
            let state = estado_de_teste().await;
            let resp = job_run_logs(state.clone(), "jrun_T".to_string());
            let body = resp.into_body();
            // O assinante do bus nasce numa tarefa: dá a vez a ela antes de publicar.
            tokio::time::sleep(Duration::from_millis(100)).await;

            const N: usize = 413;
            let coletor = tokio::spawn(linhas_do_sse(body, N, Duration::from_secs(5)));
            for i in 0..N {
                state.bus.publish(shared::Event::JobLogLine {
                    job_run_id: "jrun_T".into(),
                    job_id: "job_1".into(),
                    line: format!("linha {i}"),
                    timestamp: chrono::Utc::now(),
                    stream: shared::protocol::LogStream::Stdout,
                });
                if espacamento_us == 0 {
                    tokio::task::yield_now().await;
                } else {
                    tokio::time::sleep(Duration::from_micros(espacamento_us)).await;
                }
            }
            let linhas = coletor.await.unwrap();
            let esperadas: Vec<String> = (0..N).map(|i| format!("linha {i}")).collect();
            assert_eq!(
                linhas.len(),
                esperadas.len(),
                "espaçamento {espacamento_us}µs: faltaram {} linhas",
                esperadas.len().saturating_sub(linhas.len())
            );
            assert_eq!(
                linhas, esperadas,
                "espaçamento {espacamento_us}µs: fora de ordem"
            );
        }
    }

    fn linha_bd(i: i64, base: chrono::DateTime<chrono::Utc>) -> shared::protocol::BuildLogLine {
        shared::protocol::BuildLogLine {
            stream: shared::protocol::LogStream::Stdout,
            line: format!("linha {i}"),
            timestamp: base + chrono::Duration::microseconds(i),
        }
    }

    /// A recuperação só repõe o que o consumidor ainda não recebeu e é posterior ao
    /// momento em que o stream conectou (o resto é histórico do seed da janela).
    #[test]
    fn recover_lines_repoe_so_o_que_falta_depois_de_conectar() {
        let base = chrono::Utc::now();
        let mut seen = SeenLines::new();
        // Já entregues pelo bus: 2 e 3.
        assert!(mark_seen(
            &mut seen,
            base + chrono::Duration::microseconds(2),
            "linha 2"
        ));
        assert!(mark_seen(
            &mut seen,
            base + chrono::Duration::microseconds(3),
            "linha 3"
        ));
        let no_banco: Vec<_> = (0..6).map(|i| linha_bd(i, base)).collect();

        // Conectou em `base + 1µs`: a linha 0 é do histórico (seed) e fica de fora.
        let since = base + chrono::Duration::microseconds(1);
        let repostas: Vec<String> = recover_lines(no_banco.clone(), since, &mut seen)
            .into_iter()
            .map(|l| l.line)
            .collect();
        assert_eq!(repostas, ["linha 1", "linha 4", "linha 5"]);

        // Repetir a recuperação não reenvia nada.
        assert!(recover_lines(no_banco, since, &mut seen).is_empty());
    }

    /// Quando o consumidor atrasa e o canal descarta eventos (o `Lagged`), o stream
    /// repõe as linhas pelo banco: todas saem, UMA vez só e em ordem.
    #[tokio::test]
    async fn sse_de_job_repoe_pelo_banco_o_que_o_canal_descartou() {
        use std::time::Duration;
        let state = estado_de_teste().await;
        let resp = job_run_logs(state.clone(), "jrun_L".to_string());
        let body = resp.into_body();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Bem mais que a capacidade do canal (1024). Nada cede a vez ao consumidor
        // durante a publicação (runtime de uma thread só): ele atrasa de propósito.
        const N: i64 = 5000;
        let base = chrono::Utc::now();
        for i in 0..N {
            let l = linha_bd(i, base);
            crate::store::job_log::append(&state.db, "jrun_L", &l.stream, &l.line, l.timestamp)
                .await
                .unwrap();
        }
        for i in 0..N {
            let l = linha_bd(i, base);
            state.bus.publish(shared::Event::JobLogLine {
                job_run_id: "jrun_L".into(),
                job_id: "job_1".into(),
                line: l.line,
                timestamp: l.timestamp,
                stream: l.stream,
            });
        }

        let linhas = linhas_do_sse(body, N as usize, Duration::from_secs(10)).await;
        let esperadas: Vec<String> = (0..N).map(|i| format!("linha {i}")).collect();
        assert_eq!(linhas.len(), esperadas.len(), "faltaram ou sobraram linhas");
        assert_eq!(linhas, esperadas, "fora de ordem ou duplicadas");
    }

    /// O token certo passa com ou sem o prefixo `Bearer`; o motivo da recusa
    /// distingue header ausente, esquema errado e token divergente — é o que
    /// torna um 401 diagnosticável no journal do daemon.
    #[test]
    fn auth_check_aceita_token_certo_e_explica_a_recusa() {
        let esperado = "s3cr3t-token";

        assert!(auth_check(&req_com_auth(Some("Bearer s3cr3t-token")), esperado).is_ok());
        assert!(auth_check(&req_com_auth(Some("bearer s3cr3t-token")), esperado).is_ok());
        assert!(auth_check(&req_com_auth(Some("s3cr3t-token")), esperado).is_ok());

        let e = auth_check(&req_com_auth(None), esperado).unwrap_err();
        assert!(e.contains("sem header"), "{e}");

        let e = auth_check(&req_com_auth(Some("Basic s3cr3t-token")), esperado).unwrap_err();
        assert!(e.contains("esquema"), "{e}");

        // Comprimento diferente = token truncado/com lixo colado junto: o
        // motivo cita os dois tamanhos (nunca o token).
        let e = auth_check(&req_com_auth(Some("Bearer s3cr3t")), esperado).unwrap_err();
        assert!(e.contains("6 bytes") && e.contains("12 bytes"), "{e}");
        assert!(
            !e.contains("s3cr3t"),
            "o motivo não pode vazar o token: {e}"
        );

        // Mesmo comprimento, conteúdo diferente = token de outra instalação.
        let e = auth_check(&req_com_auth(Some("Bearer 0000000token")), esperado).unwrap_err();
        assert!(e.contains("mesmo comprimento"), "{e}");
    }

    fn content_encoding(resp: &Response<ApiBody>) -> Option<String> {
        resp.headers()
            .get(hyper::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    /// `json_response` comprime só quando o cliente aceita gzip E o corpo passa
    /// de `GZIP_MIN`; senão manda cru (sem `Content-Encoding`). E o gzip é
    /// reversível — o cliente recupera o JSON original.
    #[test]
    fn json_response_comprime_grande_e_deixa_pequeno_cru() {
        use std::io::Read;

        let big = vec![b'{'; GZIP_MIN + 10]; // >= limiar, comprime bem (repetido)
        let small = b"{\"ok\":true}".to_vec(); // < limiar

        // Grande + aceita gzip → Content-Encoding: gzip, e descomprime de volta.
        let resp = json_response(big.clone(), true);
        assert_eq!(content_encoding(&resp).as_deref(), Some("gzip"));

        // Corpo pequeno → nunca comprime (não vale o overhead).
        assert_eq!(content_encoding(&json_response(small.clone(), true)), None);
        // Cliente não aceita gzip → nunca comprime, mesmo grande.
        assert_eq!(content_encoding(&json_response(big.clone(), false)), None);

        // Round-trip: o corpo gzipado descomprime no conteúdo original.
        let gz = gzip(&big).unwrap();
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, big);
    }
}
