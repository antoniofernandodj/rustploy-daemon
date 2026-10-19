//! Servidor de estáticos da web UI/PWA (`crates/daemon/webui/`) — alternativa
//! ao client iced (`rustploy-gui`), que estava sendo bloqueado pelo Windows
//! Defender. Fala com o daemon pelo MESMO protocolo HTTP/JSON + SSE que o
//! client iced já usa (`POST /api/rpc`, `GET /api/events`) — este módulo só
//! entrega o app shell (HTML/CSS/JS/ícones), pré-minificado e pré-gzipado em
//! tempo de build (ver `../../build.rs`), sem I/O nem CPU extra em runtime.
//!
//! Roteamento por caminho fixo, plugado em `http/mod.rs::handle` ANTES do
//! gate de Bearer token — mesmo tratamento das rotas públicas de webhook/
//! OAuth, porque o usuário ainda não tem o token quando a página carrega (ele
//! loga *depois* de abrir). Isso é seguro: só entrega estático, sem dado
//! nenhum — todo acesso a dado de verdade continua exigindo o Bearer em
//! `/api/rpc` e `/api/events`, como já fazia o client iced.

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Response, StatusCode};

type ApiBody = BoxBody<Bytes, std::convert::Infallible>;

/// Um arquivo do app shell, já processado (minificado + gzipado) em tempo de
/// build — ver `Asset` gerado por `build.rs` em `$OUT_DIR/webui_assets.rs`.
pub struct Asset {
    pub route: &'static str,
    pub content_type: &'static str,
    /// Hash de conteúdo (FNV-1a hex), usado como `ETag`.
    pub etag: &'static str,
    /// `true` para o HTML de entrada e o service worker: precisam revalidar
    /// sempre, para uma atualização do daemon propagar sem exigir
    /// hard-refresh. Os demais assets são imutáveis (o conteúdo só muda
    /// quando o build muda, e nesse caso são simplesmente outros bytes na
    /// mesma rota — o `ETag` cobre esse caso).
    pub no_cache: bool,
    /// Bytes já comprimidos com gzip.
    pub gz: &'static [u8],
}

static ASSETS: &[Asset] = include!(concat!(env!("OUT_DIR"), "/webui_assets.rs"));

/// Serve `path` se casar com algum asset embutido do app shell; `None` se a
/// rota não pertencer à web UI (o chamador cai no 404 padrão da API).
pub fn serve(path: &str) -> Option<Response<ApiBody>> {
    let asset = ASSETS.iter().find(|a| a.route == path)?;
    let cache_control = if asset.no_cache {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    Some(
        Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, asset.content_type)
            .header(hyper::header::CONTENT_ENCODING, "gzip")
            .header(hyper::header::CACHE_CONTROL, cache_control)
            .header(hyper::header::ETAG, format!("\"{}\"", asset.etag))
            .body(Full::new(Bytes::from_static(asset.gz)).boxed())
            .expect("resposta estática bem-formada"),
    )
}

/// Fase 0 do plano de convergência de templates (`docs/plano-convergencia-
/// templates-gui-webui.md`): equivalente web de
/// `crates/rustploy-gui/tests/templates_render.rs` — em vez de reimplementar
/// um servidor de estáticos, serve os MESMOS bytes que o daemon serve em
/// produção (`serve()` acima, já gzipados/minificados pelo `build.rs`) e
/// dirige um Chrome headless de verdade contra eles. Não passa por login/SSE
/// — como o teste iced, semeia o estado direto (aqui, `Alpine.store('app')`
/// em vez de `GlacierUI::define_data`) e varre as telas.
#[cfg(test)]
mod headless_tests {
    use super::serve;
    use bytes::Bytes;
    use chromiumoxide::browser::{Browser, BrowserConfig};
    use futures::StreamExt;
    use http_body_util::{BodyExt, Full, combinators::BoxBody};
    use hyper::{Request, Response, StatusCode, server::conn::http1, service::service_fn};
    use hyper_util::rt::TokioIo;
    use std::time::Duration;
    use tokio::net::TcpListener;

    type ApiBody = BoxBody<Bytes, std::convert::Infallible>;

    /// Sobe um servidor HTTP mínimo (mesmo padrão de accept-loop de
    /// `http/mod.rs::listen`) que só chama `serve()` — os mesmos bytes que o
    /// daemon real serve, sem precisar de `AppState` (Docker/DB/etc.), que
    /// `serve()` não usa.
    async fn spawn_static_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind do servidor estático de teste");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    let svc = service_fn(|req: Request<hyper::body::Incoming>| async move {
                        let resp: Response<ApiBody> =
                            serve(req.uri().path()).unwrap_or_else(|| {
                                Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::from_static(b"not found")).boxed())
                                    .expect("404 bem-formado")
                            });
                        Ok::<_, std::convert::Infallible>(resp)
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        addr
    }

    /// `cargo test` roda os 3 testes deste módulo em threads concorrentes;
    /// sem um `user_data_dir` próprio, todos os Chrome apontam pro mesmo
    /// perfil default do chromiumoxide e o segundo a chegar morre com
    /// "Failed to create SingletonLock: File exists" — cada `launch()`
    /// precisa do seu diretório.
    static USER_DATA_DIR_COUNTER: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0);

    async fn launch() -> (Browser, tokio::task::JoinHandle<()>) {
        let n = USER_DATA_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let user_data_dir =
            std::env::temp_dir().join(format!("rustploy-webui-test-{}-{n}", std::process::id()));
        let config = BrowserConfig::builder()
            .chrome_executable("/usr/bin/google-chrome")
            .no_sandbox()
            .user_data_dir(user_data_dir)
            .build()
            .expect("BrowserConfig");
        let (browser, mut handler) = Browser::launch(config)
            .await
            .expect("google-chrome deve estar instalado (ver docs/inventario-paridade-telas.md)");
        // A stream de eventos CDP precisa ser drenada continuamente — sem
        // isso nenhum comando (navigate/evaluate) recebe resposta. Mesmo
        // padrão do exemplo oficial do chromiumoxide.
        let h = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                let _ = ev;
            }
        });
        (browser, h)
    }

    /// Injeta um coletor de erros ANTES de qualquer script da página rodar
    /// (`Page.addScriptToEvaluateOnNewDocument`), pra pegar erros de módulo
    /// ES (`app.js`) que disparam antes do nosso primeiro `evaluate()`.
    const ERROR_COLLECTOR_JS: &str = r#"
        window.__errors = [];
        window.addEventListener('error', (e) => window.__errors.push(String(e.message || e.error)));
        window.addEventListener('unhandledrejection', (e) => window.__errors.push('rejection: ' + String(e.reason)));
    "#;

    /// Abre uma aba em branco, registra o coletor de erros e SÓ ENTÃO navega
    /// pra `index.html` — nessa ordem, pra `evaluate_on_new_document` valer
    /// já no primeiro load (`browser.new_page(url)` navegaria direto, o que
    /// deixaria a carga inicial sem o coletor).
    async fn open_page(browser: &Browser, addr: std::net::SocketAddr) -> chromiumoxide::Page {
        let page = browser.new_page("about:blank").await.expect("new_page");
        page.evaluate_on_new_document(ERROR_COLLECTOR_JS)
            .await
            .expect("injeta o coletor de erros");
        page.goto(format!("http://{addr}/")).await.expect("goto");
        let _ = page.wait_for_navigation().await;
        page
    }

    /// Espera até `Alpine.store('app')` existir — só depois do evento
    /// `load` (ver o comentário no fim de `webui/app.js`: os módulos
    /// registram os `Alpine.data`/`Alpine.store` primeiro, `Alpine.start()`
    /// só dispara depois).
    async fn wait_alpine_ready(page: &chromiumoxide::Page) {
        for _ in 0..100 {
            if let Ok(r) = page
                .evaluate("typeof window.Alpine !== 'undefined' && !!Alpine.store('app')")
                .await
                && r.into_value::<bool>().unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("Alpine.store('app') não ficou pronto a tempo");
    }

    /// Snapshot mínimo — mesmo formato que `http/mod.rs::snapshot()` monta e
    /// `app.js::applySnapshot` consome (`kind` é ignorado por `applySnapshot`,
    /// só o `onStreamEvent` do SSE real olha pra ele). Cobre o suficiente
    /// pra cada tela ter pelo menos uma linha de tabela — mesmo espírito das
    /// fixtures de `templates_render.rs`, sem tentar ser exaustivo.
    const SNAPSHOT_JS: &str = r#"
        {
            status: { version: '0.0.0-test', uptime_secs: 3600, services_running: 1, services_total: 1 },
            deployments: [{ service_name: 'api', project_name: 'acme',
                deployment: { state: 'Live', started_at: new Date().toISOString() } }],
            projects: [{ id: 'prj_1', name: 'acme', description: '', env_vars: [], env_comments: [], secrets: ['GITHUB_TOKEN'] }],
            services: [{ project_name: 'acme', service: {
                id: 'svc_1', status: 'Running', live_container_id: 'abc123', containers: [],
                spec: { name: 'api', project_id: 'prj_1', port: 8080, replicas: 1, env_vars: [], domains: [],
                    source: { Git: { url: 'https://github.com/acme/api.git', branch: 'main', root_path: '.',
                        watch_paths: [], submodules: false, dockerfile_path: 'Dockerfile', build_context: '.',
                        build_stage: null, credentials: null, username: null, provider_id: null } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } } } }],
            docker_images: [], docker_volumes: [], docker_networks: [], docker_containers: [],
            jobs: [], engine: { paused: false, queued: [], active: [], recent: [] },
            registry_status: { enabled: true, domain: '', port: 5100 }, registry_repos: [],
        }
    "#;

    /// Semeia a sessão (bypassa login/SSE, mesma ideia de `boot()` +
    /// `define_data` em `templates_render.rs`) e roda `applySnapshot` — o
    /// mesmo método que o SSE real chama, então `dataLoading`/`jobsById`
    /// saem consistentes.
    async fn seed_connected(page: &chromiumoxide::Page) {
        wait_alpine_ready(page).await;
        let js = format!(
            r#"(() => {{
                const s = Alpine.store('app');
                s.connected = true;
                s.screen = 'shell';
                s.api = {{ baseUrl: location.origin, token: 't',
                    rpc: async () => ({{ ok: false, error: 'stub' }}),
                    rpcChecked: async () => ({{ ok: false, error: 'stub' }}) }};
                s.applySnapshot({SNAPSHOT_JS});
            }})()"#
        );
        page.evaluate(js).await.expect("seed do Alpine store");
    }

    /// Avalia `js` (uma expressão, possivelmente uma Promise) e devolve o
    /// resultado como `String` — os testes serializam com `JSON.stringify`.
    async fn eval_str(page: &chromiumoxide::Page, js: &str) -> String {
        page.evaluate(js)
            .await
            .unwrap_or_else(|e| panic!("evaluate falhou: {e}\n{js}"))
            .into_value::<String>()
            .expect("o JS deveria devolver uma string")
    }

    /// `true` se o elemento existir e estiver de fato visível (`x-show`
    /// escreve `display:none` inline; `offsetParent` é `null` tanto pra
    /// `display:none` quanto pra ancestral escondido — cobre os dois).
    ///
    /// Faz polling (não uma checagem só): Alpine aplica `x-show`/`x-if` num
    /// microtask agendado pelo próprio scheduler dele, não sincronamente na
    /// mutação do store — então um `evaluate()` (round-trip CDP) imediatamente
    /// depois de mudar `view` pode chegar antes desse microtask rodar. Sem
    /// isso, o teste ficava flaky (falhava numa tela aleatória do sweep a
    /// cada corrida).
    async fn visible(page: &chromiumoxide::Page, selector: &str) -> bool {
        let js = format!(
            "(() => {{ const el = document.querySelector({selector:?}); return !!el && el.offsetParent !== null; }})()"
        );
        for _ in 0..20 {
            let ok = page
                .evaluate(js.as_str())
                .await
                .ok()
                .and_then(|r| r.into_value::<bool>().ok())
                .unwrap_or(false);
            if ok {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    async fn errors(page: &chromiumoxide::Page) -> Vec<String> {
        page.evaluate("window.__errors || []")
            .await
            .ok()
            .and_then(|r| r.into_value::<Vec<String>>().ok())
            .unwrap_or_default()
    }

    /// Login por token de conexão: `connect()` decodifica o base64 de
    /// `host:porta::token` e usa o token da API direto como Bearer — sem
    /// `/api/login`; o que não decodifica vira erro na tela. O `fetch` é substituído na
    /// página (o servidor de teste só serve estáticos); o SSE fica pendurado
    /// para a sessão não cair de volta no login.
    #[tokio::test]
    async fn login_with_access_key() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        wait_alpine_ready(&page).await;

        page.evaluate(
            r#"(() => {
                window.__requests = [];
                window.fetch = (url, init = {}) => {
                    const path = new URL(url, location.origin).pathname;
                    window.__requests.push({ path, body: init.body || null,
                        auth: (init.headers || {})["Authorization"] || null });
                    const reply = (status, body) => Promise.resolve(new Response(JSON.stringify(body), { status }));
                    if (path === "/api/events") return new Promise(() => {});
                    if (path === "/api/rpc") return reply(200, { DaemonStatus: { version: "t" } });
                    return reply(404, {});
                };
            })()"#,
        )
        .await
        .expect("stub do fetch");

        // Recusa: a mensagem do daemon chega ao usuário, a tela não muda.
        eval_str(
            &page,
            r#"(async () => { const s = Alpine.store('app'); s.accessKey = 'segredo'; await s.connect(); return 'ok'; })()"#,
        )
        .await;
        assert_eq!(
            eval_str(&page, "Alpine.store('app').error").await,
            "isso não é um token de conexão — cole o token de conexão inteiro (base64 de host:porta::token), não só o token da API"
        );
        assert_eq!(eval_str(&page, "Alpine.store('app').screen").await, "login");

        // Sucesso: o token devolvido vira o Bearer das chamadas seguintes.
        eval_str(
            &page,
            r#"(async () => { const s = Alpine.store('app');
                s.accessKey = 'MTI3LjAuMC4xOjk6Ong=';
                await s.connect(); return 'ok'; })()"#,
        )
        .await;
        assert_eq!(eval_str(&page, "Alpine.store('app').screen").await, "shell");
        assert_eq!(
            eval_str(
                &page,
                "window.__requests.find(r => r.path === '/api/rpc').auth"
            )
            .await,
            "Bearer x"
        );
        assert!(
            !eval_str(&page, "JSON.stringify(window.__requests.map(r => r.path))")
                .await
                .contains("/api/login"),
            "a webui não passa mais pelo /api/login"
        );
        // O token de conexão para copiar em Settings é montado da sessão (origem + token).
        let chave = eval_str(&page, "Alpine.store('app').serverAccessKey").await;
        {
            use base64::Engine;
            let payload = base64::engine::general_purpose::STANDARD
                .decode(&chave)
                .expect("token de conexão em base64 padrão");
            assert_eq!(String::from_utf8(payload).unwrap(), format!("{addr}::x"));
        }
        let comando = eval_str(&page, "Alpine.store('app').serverMcpCommand").await;
        assert!(
            comando.contains("/mcp") && comando.contains("Bearer x"),
            "{comando}"
        );
        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Varre as telas de nível superior (equivalente ao loop `for view in
    /// [...]` de `templates_render.rs::all_screens_and_service_tabs_render`)
    /// e confirma que cada uma monta (`x-data` fica visível) sem erro de JS.
    /// `support` não existe na webui (só no client iced — ver
    /// `docs/inventario-paridade-telas.md`), então não entra aqui.
    #[tokio::test]
    async fn all_main_screens_render() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;

        // Login (tela default, antes de qualquer seed).
        wait_alpine_ready(&page).await;
        assert!(
            visible(&page, ".login").await,
            "tela de login deveria estar visível antes do login"
        );

        seed_connected(&page).await;

        let views: &[(&str, &str)] = &[
            ("deployments", "[x-data=\"dashboard\"]"),
            ("projects", "[x-data=\"projects\"]"),
            ("monitoring", "[x-data=\"monitoring\"]"),
            ("ingress", "[x-data=\"ingress\"]"),
            ("docker", "[x-data=\"docker\"]"),
            ("schedules", "[x-data=\"schedules\"]"),
            ("settings", "[x-data=\"settings\"]"),
            ("deploy_engine", "[x-data=\"deployEngine\"]"),
        ];
        for (view, selector) in views {
            page.evaluate(format!("Alpine.store('app').view = {view:?}"))
                .await
                .unwrap_or_else(|e| panic!("set view={view}: {e}"));
            assert!(
                visible(&page, selector).await,
                "view={view} deveria estar visível (seletor {selector})"
            );
        }
        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Aba "Projeto aberto" — `view` chama-se `project` na webui contra
    /// `project_services` no client iced (divergência mecânica de nome, ver
    /// `docs/inventario-paridade-telas.md`); precisa de `selectedProjectId`.
    #[tokio::test]
    async fn project_detail_renders() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        page.evaluate(
            "Alpine.store('app').view = 'project'; Alpine.store('app').selectedProjectId = 'prj_1'",
        )
        .await
        .expect("abre o projeto");
        assert!(
            visible(&page, "[x-data=\"projectDetail\"]").await,
            "project_detail deveria estar visível"
        );
        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Detalhe de serviço + a aba General (edição de origem — Compose / Git
    /// / conta conectada / Zip), implementada nesta sessão. `serviceDetail`
    /// não é derivado de `snap` (o real vem de `fetchServiceDetail`), então
    /// precisa ser semeado à mão — mesma razão de `m.define_data("service_env",
    /// ...)` no teste iced.
    #[tokio::test]
    async fn service_detail_and_general_tab_render() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        let open_service = r#"(() => {
            const s = Alpine.store('app');
            s.view = 'service';
            s.selectedServiceId = 'svc_1';
            s.serviceTab = 'general';
            s.serviceLoading = false;
            s.serviceDeployments = [];
            s.serviceDetail = {
                id: 'svc_1', status: 'Running', live_container_id: 'abc123',
                spec: { name: 'api', port: 8080, replicas: 1, project_id: 'prj_1', env_vars: [], domains: [],
                    source: { Git: { url: 'https://github.com/acme/api.git', branch: 'main', root_path: '.',
                        watch_paths: ['src'], submodules: false, dockerfile_path: 'Dockerfile', build_context: '.',
                        build_stage: null, credentials: null, username: null, provider_id: null } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
            };
        })()"#;
        page.evaluate(open_service).await.expect("abre o serviço");
        assert!(
            visible(&page, "[x-data=\"serviceDetail\"]").await,
            "service_detail deveria estar visível"
        );
        assert!(
            visible(
                &page,
                "[x-data=\"serviceDetail\"] input[x-model=\"serviceFormRepoUrl\"]"
            )
            .await,
            "aba General deveria mostrar o form Git (sourceKind=Git)"
        );

        // As 3 sub-abas Provider (mesma cobertura do sweep de prov_tab em
        // templates_render.rs) — cada uma com um marcador só seu no DOM.
        let prov_tabs: &[(&str, &str)] = &[
            (
                "git",
                "[x-data=\"serviceDetail\"] input[x-model=\"serviceFormRepoUrl\"]",
            ),
            (
                "gitea",
                "[x-data=\"serviceDetail\"] select[x-model=\"giteaProviderId\"]",
            ),
            ("zip", "[x-data=\"serviceDetail\"] input[type=\"file\"]"),
        ];
        for (tab, selector) in prov_tabs {
            page.evaluate(format!(
                "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]')).setProviderTab({tab:?})"
            ))
            .await
            .unwrap_or_else(|e| panic!("setProviderTab({tab}): {e}"));
            assert!(
                visible(&page, selector).await,
                "prov_tab={tab} deveria mostrar {selector}"
            );
        }

        // Compose: troca a origem inteira e confirma que o textarea aparece.
        page.evaluate(
            r#"(() => {
                const s = Alpine.store('app');
                s.serviceDetail = JSON.parse(JSON.stringify(s.serviceDetail));
                s.serviceDetail.spec.source = { Compose: { content: 'services:\n  web:\n    image: nginx\n' } };
                Alpine.$data(document.querySelector('[x-data="serviceDetail"]')).initGeneralForm();
            })()"#,
        )
        .await
        .expect("troca pra origem Compose");
        // Seletor ESCOPADO ao componente: `composeText` também é o nome do
        // campo local do wizard "Novo serviço" (`newService`, passo Compose
        // colado) — `document.querySelector` sem escopo pegava aquele
        // textarea (fora de tela, `x-show=false`) em vez do nosso.
        assert!(
            visible(
                &page,
                "[x-data=\"serviceDetail\"] textarea[x-model=\"composeText\"]"
            )
            .await,
            "origem Compose deveria mostrar o editor de texto"
        );

        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Renomear serviço (aba General): o campo aparece, o aviso muda conforme o
    /// tipo (Compose × Application), `renameService()` manda o spec inteiro só
    /// com o nome trocado, não manda nada se o nome não mudou, e o card
    /// "Internal URL" de um Compose continua apontando para a chave do YAML
    /// depois do rename (o hostname interno não acompanha o nome).
    #[tokio::test]
    async fn rename_service_in_general_tab() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        page.evaluate(
            r#"(() => {
                const s = Alpine.store('app');
                s.view = 'service';
                s.selectedServiceId = 'svc_1';
                s.serviceTab = 'general';
                s.serviceLoading = false;
                s.serviceDeployments = [];
                s.serviceDetail = {
                    id: 'svc_1', status: 'Running', live_container_id: null,
                    spec: { name: 'banco', port: 5432, replicas: 1, project_id: 'prj_1', db_kind: 'postgres',
                        env_vars: [], domains: [],
                        source: { Compose: { content: 'services:\n  # comentário\n\n  rp_banco:\n    image: postgres:18\n' } },
                        healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
                };
                // Captura o que o rename manda salvar, sem daemon.
                window.__saved = [];
                s.saveServiceSpec = async (spec, msg) => { window.__saved.push({ spec, msg }); return { ok: true }; };
                window.__toasts = [];
                s.toastError = (m) => window.__toasts.push(m);
                Alpine.$data(document.querySelector('[x-data="serviceDetail"]')).initGeneralForm();
            })()"#,
        )
        .await
        .expect("abre o serviço Compose");

        assert!(
            visible(
                &page,
                "[x-data=\"serviceDetail\"] input[x-model=\"editName\"]"
            )
            .await,
            "a aba General deveria mostrar o campo de nome"
        );
        let d = "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]'))";
        assert_eq!(
            eval_str(&page, "(() => Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]')).editName)()").await,
            "banco",
            "o campo começa com o nome atual"
        );
        assert!(
            eval_str(&page, "(() => Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]')).renameNote)()")
                .await
                .contains("volumes"),
            "Compose: o aviso diz que a stack e os volumes não mudam"
        );

        // Mesmo nome e nome vazio: nada é salvo; o vazio avisa.
        let _ = eval_str(
            &page,
            &format!("(async () => {{ const d = {d}; d.editName = 'banco'; await d.renameService(); d.editName = '   '; await d.renameService(); return ''; }})()")
        )
        .await;
        assert_eq!(
            eval_str(&page, "(() => JSON.stringify({ saved: window.__saved.length, toasts: window.__toasts }))()").await,
            r#"{"saved":0,"toasts":["Informe o nome do serviço"]}"#,
            "mesmo nome não salva; vazio só avisa"
        );

        // Nome novo: manda o spec inteiro (fonte, porta, db_kind) só com o nome trocado.
        let saved = eval_str(
            &page,
            &format!("(async () => {{ const d = {d}; d.editName = '  meu-banco  '; await d.renameService(); const x = window.__saved[0]; return JSON.stringify({{ n: window.__saved.length, msg: x.msg, name: x.spec.name, port: x.spec.port, kind: x.spec.db_kind, content: x.spec.source.Compose.content, edit: d.editName }}); }})()")
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(v["n"], 1);
        assert_eq!(v["msg"], "serviço renomeado");
        assert_eq!(v["name"], "meu-banco", "o nome é aparado");
        assert_eq!(v["port"], 5432);
        assert_eq!(v["kind"], "postgres");
        assert!(v["content"].as_str().unwrap().contains("rp_banco:"));
        assert_eq!(v["edit"], "meu-banco");

        // O detalhe passa a ter o nome novo; o host interno segue o YAML.
        let url = eval_str(
            &page,
            &format!("(() => {{ Alpine.store('app').serviceDetail.spec.name = 'meu-banco'; {d}.connectionUrl = 'postgresql://u:p@rp_banco:5432/db'; return {d}.connectionInfo.internalUrl; }})()")
        )
        .await;
        // O host (chave do YAML) agora é decidido no daemon
        // (`crate::platform::connection::compose_host`, testado lá); a webui só exibe.
        assert_eq!(url, "postgresql://u:p@rp_banco:5432/db");

        // Application: outro aviso.
        let app = eval_str(
            &page,
            &format!("(() => {{ {d}.connectionUrl = ''; const s = Alpine.store('app'); s.serviceDetail = {{ id: 'svc_2', status: 'Running', live_container_id: null, spec: {{ name: 'meu-api', port: 8080, replicas: 1, project_id: 'prj_1', env_vars: [], domains: [], source: {{ Registry: {{ image: 'nginx' }} }}, healthcheck: {{ kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 }} }} }}; return JSON.stringify({{ note: {d}.renameNote, url: {d}.connectionInfo.internalUrl }}); }})()")
        )
        .await;
        let a: serde_json::Value = serde_json::from_str(&app).unwrap();
        assert!(a["note"].as_str().unwrap().contains("próximo deploy"));
        assert_eq!(a["url"], "—", "sem a URL do daemon, não inventa uma");

        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Toasts (`$store.app.toast`): o feedback de ação da webui, que até
    /// então só existia na GUI desktop. Cobre o que a mudança promete —
    /// aparecer com a cor do kind, empilhar, sair no clique do "×" e expirar
    /// sozinho — porque nada disso passa por um snapshot ou por uma rota: é
    /// só store + template, e um `cargo test` verde sem navegador não diria
    /// nada sobre ele.
    #[tokio::test]
    async fn toasts_appear_stack_and_expire() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        page.evaluate(
            r#"(() => {
                const s = Alpine.store('app');
                s.toastOk('salvo');
                s.toastError('erro: falhou');
                s.toastWarn('confira os campos');
            })()"#,
        )
        .await
        .expect("dispara três toasts");

        assert!(
            visible(&page, ".toast-wrap .toast--success").await,
            "toast de sucesso deveria aparecer"
        );
        for kind in ["success", "error", "warning"] {
            assert!(
                visible(&page, &format!(".toast-wrap .toast--{kind}")).await,
                "toast kind={kind} deveria aparecer"
            );
        }
        let count = page
            .evaluate("document.querySelectorAll('.toast-wrap .toast').length")
            .await
            .expect("conta os toasts")
            .into_value::<u32>()
            .expect("contagem numérica");
        assert_eq!(
            count, 3,
            "os três toasts deveriam empilhar, não se substituir"
        );

        // O "×" dispensa só o seu.
        page.evaluate("document.querySelector('.toast-wrap .toast--error .toast__close').click()")
            .await
            .expect("clica no × do toast de erro");
        for _ in 0..20 {
            let n = page
                .evaluate("document.querySelectorAll('.toast-wrap .toast').length")
                .await
                .ok()
                .and_then(|r| r.into_value::<u32>().ok())
                .unwrap_or(9);
            if n == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let restantes = page
            .evaluate("[...document.querySelectorAll('.toast-wrap .toast')].map(e => e.className).join(' ')")
            .await
            .expect("classes restantes")
            .into_value::<String>()
            .expect("string");
        assert!(
            !restantes.contains("toast--error"),
            "o × deveria remover o toast clicado; restou: {restantes}"
        );
        assert!(
            restantes.contains("toast--success") && restantes.contains("toast--warning"),
            "os outros dois deveriam continuar; restou: {restantes}"
        );

        // Expiração automática: duração curta em vez de esperar os 4s padrão.
        page.evaluate("Alpine.store('app').toast('some sozinho', 'info', 150)")
            .await
            .expect("toast curto");
        assert!(
            visible(&page, ".toast-wrap .toast--info").await,
            "toast info deveria aparecer antes de expirar"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        let sobrou_info = page
            .evaluate("!!document.querySelector('.toast-wrap .toast--info')")
            .await
            .expect("consulta o toast info")
            .into_value::<bool>()
            .expect("bool");
        assert!(!sobrou_info, "o toast deveria ter expirado sozinho");

        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Servidor de banco compartilhado: a aba Databases só aparece no serviço
    /// `shared`, lista os databases e manda o `ManagedDatabaseCreate` certo.
    #[tokio::test]
    async fn shared_server_databases_tab() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        page.evaluate(
            r#"(() => {
                const s = Alpine.store('app');
                window.__rpc = [];
                s.api.rpc = async (c) => {
                    window.__rpc.push(c);
                    if (c === 'ProjectList') return { ok: true, value: { Projects: [{ id: 'prj_a', name: 'app-a' }] } };
                    if (c.ManagedDatabaseList) return { ok: true, value: { ManagedDatabases: [{ id: 'mdb_1', project_id: 'prj_a', name: 'gestao', env_var: 'DATABASE_URL', connection_url: 'postgresql://gestao:pw@rp-shared-x:5432/gestao' }] } };
                    return { ok: false, error: 'stub' };
                };
                s.api.rpcChecked = async (c) => { window.__rpc.push(c); return { ok: true, value: {} }; };
                s.view = 'service';
                s.selectedServiceId = 'svc_1';
                s.serviceTab = 'databases';
                s.serviceLoading = false;
                s.serviceDeployments = [];
                s.toastOk = () => {};
                s.serviceDetail = {
                    id: 'svc_1', status: 'Running', live_container_id: null,
                    spec: { name: 'pg', port: 5432, replicas: 1, project_id: 'prj_i', db_kind: 'postgres', shared: {},
                        env_vars: [], domains: [],
                        source: { Compose: { content: 'services:\n  rp_pg:\n    image: postgres:18\n' } },
                        healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
                };
            })()"#,
        )
        .await
        .expect("abre o servidor compartilhado");

        assert!(
            visible(
                &page,
                "[x-data=\"serviceDetail\"] input[x-model=\"serviceFormSharedDatabase.name\"]"
            )
            .await,
            "a aba Databases deveria mostrar o formulário"
        );
        let d = "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]'))";
        let listed = eval_str(
            &page,
            &format!("(async () => {{ const d = {d}; await d.loadSharedState(); return JSON.stringify({{ n: d.mdbs.length, proj: d.mdbs[0].project, env: d.mdbs[0].envVar }}); }})()"),
        )
        .await;
        assert_eq!(listed, r#"{"n":1,"proj":"app-a","env":"DATABASE_URL"}"#);

        let sent = eval_str(
            &page,
            &format!("(async () => {{ const d = {d}; window.__rpc = []; d.serviceFormSharedDatabase = {{ name: ' rdo ', project: 'prj_a', env: '', limit: '20', timeout: '', overwrite: true }}; await d.createSharedDatabase(); return JSON.stringify(window.__rpc.find(c => c.ManagedDatabaseCreate).ManagedDatabaseCreate); }})()"),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["name"], "rdo", "o nome é aparado");
        assert_eq!(v["server_service_id"], "svc_1");
        assert_eq!(v["connection_limit"], 20);
        assert!(v["statement_timeout_ms"].is_null(), "vazio vira null");
        assert_eq!(v["overwrite_env"], true);

        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Aba Migrar: só aparece num banco Compose não compartilhado, lista os
    /// destinos do mesmo projeto e manda o `MigrationStart` certo.
    #[tokio::test]
    async fn migrate_old_database_tab() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        page.evaluate(
            r#"(() => {
                const s = Alpine.store('app');
                window.__rpc = [];
                s.api.rpc = async (c) => {
                    window.__rpc.push(c);
                    if (c === 'ManagedDatabaseListAll') return { ok: true, value: { ManagedDatabases: [
                        { id: 'mdb_1', project_id: 'prj_a', name: 'rdo', env_var: 'DATABASE_URL' },
                        { id: 'mdb_2', project_id: 'prj_outro', name: 'x', env_var: 'DATABASE_URL' }] } };
                    if (c.MigrationList) return { ok: true, value: { Migrations: [{ id: 'mig_1', source_service_id: 'svc_old', status: 'Completed',
                        steps: [{ name: 'Pré-checagem', state: 'ok', detail: '' }, { name: 'Dump + restore', state: 'ok', detail: '' }] }] } };
                    return { ok: false, error: 'stub' };
                };
                s.api.rpcChecked = async (c) => { window.__rpc.push(c); return { ok: true, value: {} }; };
                s.toastOk = () => {};
                s.view = 'service';
                s.selectedServiceId = 'svc_old';
                s.serviceTab = 'migrar';
                s.serviceLoading = false;
                s.serviceDeployments = [];
                s.serviceDetail = {
                    id: 'svc_old', status: 'Running', live_container_id: null,
                    spec: { name: 'banco', port: 5432, replicas: 1, project_id: 'prj_a', db_kind: 'postgres',
                        env_vars: [{ key: 'POSTGRES_DB', value: { Plain: 'meu_db' } }], domains: [],
                        source: { Compose: { content: 'services:\n  rp_banco:\n    image: postgres:18\n' } },
                        healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
                };
            })()"#,
        )
        .await
        .expect("abre o banco antigo");

        let d = "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]'))";
        let loaded = eval_str(
            &page,
            &format!("(async () => {{ const d = {d}; await d.loadMigration(); return JSON.stringify({{ dests: d.migrationDestinations.map(x => x.id), db: d.serviceFormMigration.db, st: d.mig.status, steps: d.migrationSteps.map(x => x.icon).join('') }}); }})()"),
        )
        .await;
        assert_eq!(
            loaded, r#"{"dests":["mdb_1"],"db":"meu_db","st":"Completed","steps":"✓✓"}"#,
            "só destinos do mesmo projeto; origem preenchida do POSTGRES_DB"
        );
        assert!(
            visible(
                &page,
                "[x-data=\"serviceDetail\"] input[x-model=\"serviceFormMigration.db\"]"
            )
            .await,
            "a aba Migrar deveria mostrar o formulário"
        );
        let sent = eval_str(
            &page,
            &format!("(async () => {{ window.confirm = () => true; const d = {d}; window.__rpc = []; d.serviceFormMigration.dest = 'mdb_1'; d.serviceFormMigration.env = ' URL_BANCO '; await d.startMigration(); return JSON.stringify(window.__rpc.find(c => c.MigrationStart).MigrationStart); }})()"),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["source_service_id"], "svc_old");
        assert_eq!(v["source_database"], "meu_db");
        assert_eq!(v["dest_database_id"], "mdb_1");
        assert_eq!(v["env_var"], "URL_BANCO", "a env var é aparada");

        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }
    /// A janela "Ações do job" (a lista de botões da linha não cabia): o estado
    /// quente/frio vem do daemon, o "⋯" abre a janela com as ações certas para o
    /// estado do job, e "Aquecer" num job já quente só avisa — sem disparar nada.
    #[tokio::test]
    async fn job_actions_window_lists_actions_and_warm_status() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        let seeded = eval_str(
            &page,
            &format!(
                r#"(() => {{
                    const s = Alpine.store('app');
                    const snapshot = {SNAPSHOT_JS};
                    const job = {{ id: 'job_1', project_id: 'prj_1', trigger_service_id: 'svc_1', name: 'migrator',
                        compose: 'services: {{}}', main_service: 'main', env_vars: [], env_comments: [], enabled: true,
                        recurrence: null, last_run_at: null, next_run_at: null }};
                    snapshot.jobs = [{{ job, project_name: 'acme', trigger_service_name: 'api', last_run: null,
                        warm: {{ job_id: 'job_1', warm: false, warming: false, reason: 'nunca aquecido', warmed_at: null }} }}];
                    s.applySnapshot(snapshot);
                    window.__commands = [];
                    s.api.rpcChecked = async (command) => {{
                        window.__commands.push(command);
                        return {{ ok: true, value: {{ JobWarmRequested: {{ already_warm: true }} }} }};
                    }};
                    s.view = 'schedules';
                    return 'ok';
                }})()"#
            ),
        )
        .await;
        assert_eq!(seeded, "ok");

        assert!(
            visible(&page, ".table__col-warm.state-cell--muted").await,
            "a linha deveria mostrar o job frio, com o estado vindo do daemon"
        );
        assert!(
            !visible(&page, ".job-actions").await,
            "a janela de ações começa fechada"
        );

        eval_str(
            &page,
            "(() => { document.querySelector('[title=\"Ações do job\"]').click(); return 'ok'; })()",
        )
        .await;
        assert!(
            visible(&page, ".job-actions").await,
            "o ⋯ da linha deveria abrir a janela de ações"
        );
        let items = eval_str(
            &page,
            "JSON.stringify([...document.querySelectorAll('.job-actions__item')].filter(el => el.offsetParent !== null).map(el => el.textContent.trim()))",
        )
        .await;
        assert_eq!(
            items, r#"["▶ Rodar agora","◐ Aquecer","‖ Pausar","✎ Editar","✕ Remover"]"#,
            "job parado e sem execução: sem cancelar nem logs"
        );
        assert!(
            eval_str(
                &page,
                "document.querySelector('.job-actions__status').textContent"
            )
            .await
            .contains("nunca aquecido"),
            "a janela mostra por que o job está frio"
        );

        // Quente: "Aquecer" só avisa — o daemon respondeu `already_warm`, e a tela mostra o aviso.
        eval_str(
            &page,
            r#"(() => { Alpine.store('app').snap.jobs[0].warm = { job_id: 'job_1', warm: true, warming: false, reason: null, warmed_at: new Date().toISOString() }; return 'ok'; })()"#,
        )
        .await;
        assert!(
            visible(&page, ".job-actions .state-cell--ok").await,
            "a janela relê o estado do snapshot ao vivo"
        );
        eval_str(
            &page,
            "(() => { [...document.querySelectorAll('.job-actions__item')].find(el => el.textContent.includes('Aquecer')).click(); return 'ok'; })()",
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            eval_str(&page, "JSON.stringify(window.__commands)").await,
            r#"[{"JobWarm":{"job_id":"job_1"}}]"#
        );
        assert!(
            eval_str(
                &page,
                "JSON.stringify(Alpine.store('app').toasts.map(t => t.message))"
            )
            .await
            .contains("job já está aquecido"),
            "job quente: o toast avisa que já estava aquecido"
        );

        // Em execução: cancelar no lugar de rodar, e os logs da última execução.
        eval_str(
            &page,
            r#"(() => { Alpine.store('app').snap.jobs[0].last_run = { id: 'jrun_1', success: null }; return 'ok'; })()"#,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let items = eval_str(
            &page,
            "JSON.stringify([...document.querySelectorAll('.job-actions__item')].filter(el => el.offsetParent !== null).map(el => el.textContent.trim()))",
        )
        .await;
        assert_eq!(
            items,
            r#"["■ Cancelar execução","◐ Aquecer","≡ Ver logs","‖ Pausar","✎ Editar","✕ Remover"]"#
        );

        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }

    /// Settings → Infra as Code, no mesmo fluxo do app desktop: exportar BAIXA um
    /// `.zip` com `rustploy.yml` + `rustploy.vars.toml`, escolher um `.zip` valida
    /// (exatamente um .yml e um .toml na raiz) e preenche os campos, e importar
    /// confirma antes de aplicar e mostra o prune (verbo `Deleted` nas ações).
    #[tokio::test]
    async fn iac_zip_export_import() {
        let addr = spawn_static_server().await;
        let (browser, _handler) = launch().await;
        let page = open_page(&browser, addr).await;
        seed_connected(&page).await;

        eval_str(
            &page,
            r#"(async () => {
                const s = Alpine.store('app');
                window.__confirms = [];
                window.confirm = (m) => { window.__confirms.push(m); return true; };
                window.__downloads = [];
                const click = HTMLAnchorElement.prototype.click;
                HTMLAnchorElement.prototype.click = function () {
                    if (this.download) { window.__downloads.push({ name: this.download, href: this.href }); return; }
                    return click.call(this);
                };
                s.api = { baseUrl: location.origin, token: 't',
                    rpcChecked: async (cmd) => cmd === 'ManifestExportAll'
                        ? { ok: true, value: { ManifestBundle: { yaml: 'projects: []', dotenv: '# vars' } } }
                        : { ok: false, error: 'stub' },
                    rpc: async (cmd) => { window.__import = cmd.ManifestImport;
                        return { ok: true, value: window.__importReply }; } };
                s.refreshNow = async () => {};
                s.view = 'settings'; s.settingsTab = 'iac';
                return 'ok';
            })()"#,
        )
        .await;
        assert!(
            visible(&page, "input[type=file][accept*='zip']").await,
            "a aba IaC tem o seletor de .zip"
        );
        // O botão nativo do seletor de arquivo segue o tema escuro (não o branco do navegador).
        assert_eq!(
            eval_str(
                &page,
                r#"getComputedStyle(document.querySelector("input[type=file][accept*='zip']"), '::file-selector-button').backgroundColor"#
            )
            .await,
            "rgb(33, 38, 45)",
            "botão do seletor de arquivo usa a cor do tema"
        );

        // Exportar: um download rustploy-infra.zip cujo conteúdo é o par yml/toml.
        let exported = eval_str(
            &page,
            r#"(async () => {
                await Alpine.store('app').manifestExport();
                const d = window.__downloads[0];
                const { readManifestZip } = await import('/zip.js');
                const blob = await (await fetch(d.href)).blob();
                const r = await readManifestZip(blob);
                return JSON.stringify({ name: d.name, ok: r.ok, yaml: r.yaml, toml: r.toml,
                    msg: Alpine.store('app').manifestExportMessage });
            })()"#,
        )
        .await;
        assert_eq!(
            exported,
            r##"{"name":"rustploy-infra.zip","ok":true,"yaml":"projects: []","toml":"# vars","msg":"baixado como rustploy-infra.zip"}"##
        );

        // Escolher um zip inválido (arquivo sobrando) é recusado; um válido preenche os campos.
        let picked = eval_str(
            &page,
            r#"(async () => {
                const s = Alpine.store('app');
                const { buildManifestZip } = await import('/zip.js');
                const good = new File([buildManifestZip('a: 1', 'b = 2')], 'infra.zip');
                await s.manifestPickZip({ target: { files: [new File(['isto nao e um zip'], 'x.zip')], value: '' } });
                const bad = s.manifestImportMessage;
                await s.manifestPickZip({ target: { files: [good], value: '' } });
                return JSON.stringify({ bad, yaml: s.manifestImportYaml, toml: s.manifestImportDotenv, zip: s.manifestZipName });
            })()"#,
        )
        .await;
        assert_eq!(
            picked,
            r##"{"bad":"zip inválido: não achei o fim do arquivo (não é um .zip?)","yaml":"a: 1","toml":"b = 2","zip":"infra.zip"}"##
        );

        // Importar com prune: confirma antes, manda o pedido certo e mostra o que foi removido.
        let imported = eval_str(
            &page,
            r#"(async () => {
                const s = Alpine.store('app');
                window.__importReply = { ManifestReport: { deployed: [],
                    actions: [{ kind: 'Service', name: 'loja/web', action: 'Updated' },
                              { kind: 'Service', name: 'loja/velho', action: 'Deleted' }] } };
                s.manifestPrune = true; s.manifestDeploy = true;
                await s.manifestImport();
                return JSON.stringify({ req: window.__import, confirm: window.__confirms[0],
                    lines: s.manifestReportLines, msg: s.manifestImportMessage });
            })()"#,
        )
        .await;
        let imported: serde_json::Value = serde_json::from_str(&imported).expect("json");
        assert_eq!(
            imported["req"],
            serde_json::json!({"yaml":"a: 1","dotenv":"b = 2","prune":true,"deploy":true})
        );
        let confirm = imported["confirm"].as_str().unwrap();
        assert!(
            confirm.contains("Do zip infra.zip") && confirm.contains("prune): SIM"),
            "{confirm}"
        );
        assert_eq!(
            imported["lines"],
            serde_json::json!([
                "[Updated] Service loja/web",
                "[Deleted] Service loja/velho",
                "removidos (prune): loja/velho",
                "deploy ligado — nada mudou para disparar"
            ])
        );
        assert_eq!(imported["msg"], "import concluído ✓");

        // Variável sem valor: nada é aplicado e a lista aparece.
        let missing = eval_str(
            &page,
            r#"(async () => {
                const s = Alpine.store('app');
                window.__importReply = { MissingEnvVars: ['loja/web: APP_PORT'] };
                await s.manifestImport();
                return JSON.stringify({ has: s.manifestHasMissing, vars: s.manifestMissingVars, msg: s.manifestImportMessage });
            })()"#,
        )
        .await;
        assert_eq!(
            missing,
            r##"{"has":true,"vars":"loja/web: APP_PORT","msg":"faltam variáveis — nada foi aplicado"}"##
        );

        // Cancelar na confirmação não manda nada.
        let cancelled = eval_str(
            &page,
            r#"(async () => {
                const s = Alpine.store('app');
                window.confirm = () => false; window.__import = null;
                await s.manifestImport();
                return JSON.stringify({ sent: window.__import, msg: s.manifestImportMessage });
            })()"#,
        )
        .await;
        assert_eq!(cancelled, r##"{"sent":null,"msg":"cancelado"}"##);

        assert_eq!(
            errors(&page).await,
            Vec::<String>::new(),
            "sem erros de JS no console"
        );
    }
}
