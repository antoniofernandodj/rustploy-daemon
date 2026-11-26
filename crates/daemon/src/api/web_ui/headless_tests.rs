//! Testes da web UI num Chrome sem cabeça (exigem `google-chrome` instalado): login, telas, janelas e IaC.
//!
//! Fase 0 do plano de convergência de templates (`docs/plano-convergencia-
//! templates-gui-webui.md`): equivalente web de
//! `crates/rustploy-gui/tests/templates_render.rs` — em vez de reimplementar
//! um servidor de estáticos, serve os MESMOS bytes que o daemon serve em
//! produção (`serve()` acima, já gzipados/minificados pelo `build.rs`) e
//! dirige um Chrome headless de verdade contra eles. Não passa por login/SSE
//! — como o teste iced, semeia o estado direto (aqui, `Alpine.store('app')`
//! em vez de `GlacierUI::define_data`) e varre as telas.

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
                    let resp: Response<ApiBody> = serve(req.uri().path()).unwrap_or_else(|| {
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
static USER_DATA_DIR_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

async fn launch() -> (Browser, tokio::task::JoinHandle<()>) {
    let n = USER_DATA_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let user_data_dir =
        std::env::temp_dir().join(format!("rustploy-webui-test-{}-{n}", std::process::id()));
    let config = BrowserConfig::builder()
        .chrome_executable("/usr/bin/google-chrome")
        .no_sandbox()
        // O `--headless` antigo não existe mais nos Chrome recentes (o launch estoura
        // em LaunchTimeout com stderr vazio); o modo novo é o `--headless=new`.
        .new_headless_mode()
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
        "that is not a connection token — paste the whole connection token (base64 of host:port::token), not just the API token"
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
        ("running", "[x-data=\"running\"]"),
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

/// Descrição longa do projeto: no card vêm as duas linhas cortadas e clicar
/// nelas abre um popover com o texto inteiro (fecha no clique fora); no projeto
/// aberto ela é um textarea sem borda de 3 linhas, não um input de uma linha.
#[tokio::test]
async fn long_project_description_truncates_with_popover() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            s.snap.projects[0].description = 'palavra '.repeat(40).trim();
            s.view = 'projects';
        })()"#,
    )
    .await
    .expect("descrição longa no projeto");

    let anchor = "[x-data=\"projects\"] .project-card__description--truncated";
    assert!(
        visible(&page, anchor).await,
        "a descrição longa deveria vir cortada"
    );
    let shown = eval_str(
        &page,
        &format!("(() => document.querySelector({anchor:?}).textContent)()"),
    )
    .await;
    assert_eq!(shown.chars().count(), 91, "90 caracteres + reticências");
    assert!(shown.ends_with('…'));

    // O corte é de duas linhas, e o texto fica dentro do card. A janela do teste
    // é larga demais para duas linhas do card cheias, então o grid é estreitado
    // para forçar o transbordo; as alturas saem do próprio renderizador (clamp
    // 1 = uma linha), não de um número de pixel inventado.
    let medida = eval_str(
        &page,
        r#"(() => {
            const el = document.querySelector('[x-data="projects"] .project-card__description--truncated');
            const grid = el.closest('.grid');
            const altura = () => el.getBoundingClientRect().height;
            grid.style.width = '300px';
            el.style.webkitLineClamp = '1';
            const uma = altura();
            el.style.removeProperty('-webkit-line-clamp');
            const atual = altura();
            el.style.webkitLineClamp = 'none';
            const inteira = altura();
            el.style.removeProperty('-webkit-line-clamp');
            const rect = el.getBoundingClientRect();
            const card = el.closest('.card').getBoundingClientRect();
            grid.style.removeProperty('width');
            return JSON.stringify({
                uma, atual, inteira, largura: rect.width,
                card_largura: card.width, card_altura: card.height,
            });
        })()"#,
    )
    .await;
    let m: serde_json::Value = serde_json::from_str(&medida).expect("medidas do card");
    let (uma, atual, inteira) = (
        m["uma"].as_f64().unwrap(),
        m["atual"].as_f64().unwrap(),
        m["inteira"].as_f64().unwrap(),
    );
    assert!(uma > 0.0, "uma linha do card tem altura {uma}");
    assert!(
        (atual - uma * 2.0).abs() < 1.0,
        "a descrição do card mostra duas linhas: {atual} contra uma de {uma}"
    );
    assert!(
        inteira > atual + 1.0,
        "sem o clamp o texto passaria das duas linhas do card: {inteira} vs {atual}"
    );
    assert!(
        m["largura"].as_f64().unwrap() <= m["card_largura"].as_f64().unwrap() + 0.5,
        "a descrição não passa da largura do card"
    );
    assert!(
        atual <= m["card_altura"].as_f64().unwrap(),
        "a descrição não passa da altura do card"
    );
    assert!(
        !visible(&page, "[x-data=\"projects\"] .description-popover").await,
        "o popover nasce fechado"
    );

    page.evaluate(format!("document.querySelector({anchor:?}).click()"))
        .await
        .expect("clica na descrição");
    assert!(
        visible(&page, "[x-data=\"projects\"] .description-popover").await,
        "clicar na descrição abre o popover"
    );
    assert_eq!(
        eval_str(
            &page,
            "(() => document.querySelector('[x-data=\"projects\"] .description-popover__text').textContent)()"
        )
        .await,
        "palavra ".repeat(40).trim(),
        "o popover mostra a descrição inteira"
    );

    page.evaluate("document.querySelector('[x-data=\"projects\"] .page-head__title').click()")
        .await
        .expect("clica fora");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !visible(&page, "[x-data=\"projects\"] .description-popover").await,
        "clicar fora fecha o popover"
    );

    page.evaluate(
        "Alpine.store('app').view = 'project'; Alpine.store('app').selectedProjectId = 'prj_1'",
    )
    .await
    .expect("abre o projeto");
    assert!(
        visible(
            &page,
            "[x-data=\"projectDetail\"] textarea.page-head__subtitle-textarea"
        )
        .await,
        "a descrição do projeto aberto deveria ser um textarea"
    );
    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Projeto: nome e descrição se editam no próprio título (como na GUI,
/// `project_title_change`), gravando por `ProjectUpdate` sem botão; e a
/// variável nova do projeto vira referência de secret tanto pelo modo
/// "usar secret" quanto por `<secret:NOME>` digitado (antes gravava o texto
/// puro).
#[tokio::test]
async fn project_title_and_env_secret_reference() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            s.view = 'project';
            s.selectedProjectId = 'prj_1';
            window.__saved = [];
            s.api.rpcChecked = async (cmd) => { window.__saved.push(cmd); return { ok: true }; };
            window.__env = [];
            s.saveProjectEnv = async (vars) => { window.__env.push(vars); return { ok: true }; };
        })()"#,
    )
    .await
    .expect("abre o projeto");

    assert!(
        visible(
            &page,
            "[x-data=\"projectDetail\"] input.page-head__title-input"
        )
        .await,
        "o nome do projeto deveria ser um input no título"
    );
    let d = "Alpine.$data(document.querySelector('[x-data=\"projectDetail\"]'))";
    assert_eq!(
        eval_str(&page, &format!("(() => {d}.editName)()")).await,
        "acme",
        "o título começa com o nome atual"
    );

    // Digitar grava sozinho (debounce de 0,5 s), aparando nome e descrição.
    let _ = eval_str(
        &page,
        &format!("(() => {{ const d = {d}; d.titleFocused = true; d.editName = ' acme-2 '; d.editDescription = ' loja '; d.titleInput(); return ''; }})()"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        eval_str(&page, "(() => JSON.stringify(window.__saved))()").await,
        r#"[{"ProjectUpdate":{"id":"prj_1","name":"acme-2","description":"loja"}}]"#,
        "grava o ProjectUpdate depois do debounce"
    );

    // Nome vazio nunca é gravado.
    let _ = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; d.editName = '  '; await d.titleSave(); return ''; }})()"),
    )
    .await;
    assert_eq!(
        eval_str(&page, "(() => String(window.__saved.length))()").await,
        "1"
    );

    // `<secret:NOME>` digitado e o modo "usar secret" viram { Secret }.
    let env = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; window.confirm = () => true; d.newEnvKey = 'TOKEN'; d.newEnvValue = '<secret:GITHUB_TOKEN>'; await d.addEnvVar(); d.toggleEnvSecretMode(); d.newEnvKey = 'GH'; d.newEnvValue = 'GITHUB_TOKEN'; const key = d.newEnvKey; await d.addEnvVar(); d.newEnvKey = 'PLAIN'; d.newEnvValue = 'x'; await d.addEnvVar(); return JSON.stringify({{ key, first: window.__env[0].at(-1), second: window.__env[1].at(-1), third: window.__env[2].at(-1), mode: d.newEnvIsSecret }}); }})()"),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&env).unwrap();
    assert_eq!(
        v["first"]["value"]["Secret"], "GITHUB_TOKEN",
        "<secret:NOME> digitado vira referência"
    );
    assert_eq!(v["key"], "GH");
    assert_eq!(
        v["second"]["value"]["Secret"], "GITHUB_TOKEN",
        "modo usar secret grava referência"
    );
    assert_eq!(
        v["third"]["value"]["Plain"], "x",
        "valor comum continua em claro"
    );
    assert_eq!(
        v["mode"], false,
        "o modo secret desliga depois de adicionar"
    );

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );

    // Linha editável no lugar (env_rows.js): chave e valor são campos com o que está gravado;
    // mexer não grava nada, só faz aparecer o OK; o OK confirma e grava o valor (pelo nome antigo)
    // e depois a chave.
    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            s.snap.projects[0].env_vars = [{ key: 'A', value: { Plain: '1' } }];
            s.snap = { ...s.snap };
            window.__saved = [];
        })()"#,
    )
    .await
    .expect("semeia uma variável");
    let _ = eval_str(
        &page,
        &format!("(() => {{ {d}.projectTab = 'env'; return ''; }})()"),
    )
    .await;
    assert!(
        visible(&page, "[x-data=\"projectDetail\"] input.env__key").await,
        "a chave da variável deveria ser um campo editável"
    );
    let row = eval_str(
        &page,
        &format!(
            "(async () => {{ const d = {d}; await Alpine.nextTick(); \
             const key = document.querySelector('[x-data=\"projectDetail\"] input.env__key').value; \
             const row = d.envVars.find((r) => r.key === 'A'); \
             const before = d.envRowDirty(row); \
             d.setEnvDraft(row, 'key', 'B'); d.setEnvDraft(row, 'useSecret', true); d.setEnvDraft(row, 'secret', 'GITHUB_TOKEN'); \
             const after = d.envRowDirty(row); const sentBeforeOk = window.__saved.length; \
             window.confirm = () => false; await d.saveEnvRow(row); const sentAfterNo = window.__saved.length; \
             window.confirm = () => true; await d.saveEnvRow(row); \
             return JSON.stringify({{ key, before, after, sentBeforeOk, sentAfterNo, saved: window.__saved, dirty: d.envRowDirty(row) }}); }})()"
        ),
    )
    .await;
    let r: serde_json::Value = serde_json::from_str(&row).unwrap();
    assert_eq!(r["key"], "A", "o campo da chave começa com a chave gravada");
    assert_eq!(r["before"], false, "sem mexer não há OK");
    assert_eq!(r["after"], true, "depois de mexer aparece o OK");
    assert_eq!(r["sentBeforeOk"], 0, "mexer não grava nada");
    assert_eq!(r["sentAfterNo"], 0, "recusar a confirmação não grava nada");
    assert_eq!(
        r["saved"],
        serde_json::json!([
            { "EnvValueSet": { "project_id": "prj_1", "key": "A", "value": { "Secret": "GITHUB_TOKEN" } } },
            { "EnvKeyRename": { "project_id": "prj_1", "from": "A", "to": "B" } }
        ]),
        "confirmar grava o valor pelo nome antigo e depois renomeia"
    );
    assert_eq!(r["dirty"], false, "gravado, o rascunho some");
}

/// Janela "Variáveis do job" (screens/job_env.js): a mesma linha editável do projeto e do serviço
/// (env_rows.js). A lista do "usar secret" traz os secrets do job, os do serviço gatilho e os do
/// projeto, sem repetir nome; o OK confirma e grava com o `job_id`.
#[tokio::test]
async fn job_env_editable_row() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            window.__saved = [];
            s.api.rpcChecked = async (cmd) => { window.__saved.push(cmd); return { ok: true }; };
            s.api.rpc = async (cmd) => {
                const req = cmd.SecretList;
                if (!req) return { ok: false, error: 'stub' };
                if (req.job_id) return { ok: true, value: { SecretNames: ['J'] } };
                if (req.service_id) return { ok: true, value: { SecretNames: ['S', 'J'] } };
                return { ok: true, value: { SecretNames: ['P', 'S'] } };
            };
            s.refreshNow = async () => {};
            s.jobsById = { job_1: { id: 'job_1', project_id: 'prj_1', name: 'backup', trigger_service_id: 'svc_1',
                env_vars: [{ key: 'A', value: { Plain: '1' } }], env_comments: [] } };
            s.openJobEnv('job_1');
        })()"#,
    )
    .await
    .expect("abre a janela do job");

    assert!(
        visible(&page, "[x-data=\"jobEnv\"] input.env__key").await,
        "a chave da variável do job deveria ser um campo editável"
    );
    let d = "Alpine.$data(document.querySelector('[x-data=\"jobEnv\"]'))";
    let out = eval_str(
        &page,
        &format!(
            "(async () => {{ const d = {d}; await d.loadSecrets(); \
             const choices = d.envSecretChoices.map((c) => c.name).join(','); \
             const row = d.envVars.find((r) => r.key === 'A'); \
             d.setEnvDraft(row, 'useSecret', true); d.setEnvDraft(row, 'secret', 'S'); \
             const dirty = d.envRowDirty(row); \
             window.confirm = () => false; await d.saveEnvRow(row); const sentAfterNo = window.__saved.length; \
             window.confirm = () => true; await d.saveEnvRow(row); \
             return JSON.stringify({{ choices, dirty, sentAfterNo, saved: window.__saved, after: d.envRowDirty(row) }}); }})()"
        ),
    )
    .await;
    let r: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        r["choices"], "J,S,P",
        "job, serviço gatilho e projeto, sem repetir nome"
    );
    assert_eq!(r["dirty"], true, "depois de mexer aparece o OK");
    assert_eq!(r["sentAfterNo"], 0, "recusar a confirmação não grava nada");
    assert_eq!(
        r["saved"],
        serde_json::json!([
            { "EnvValueSet": { "project_id": "prj_1", "job_id": "job_1", "key": "A", "value": { "Secret": "S" } } }
        ]),
        "confirmar grava com o job_id"
    );
    assert_eq!(r["after"], false, "gravado, o rascunho some");
    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Modal "plano de deploy": serviço com dependências mostra a ordem e três
/// botões (Confirmar / Só o serviço / Rejeitar); projeto mostra dois
/// (Aceitar / Rejeitar). Rejeitar fecha sem enfileirar nada.
#[tokio::test]
async fn deploy_plan_modal_lists_order_and_buttons() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let items = "[{service_id:'a',service_name:'db',wave:0,status:'Queued'},\
                 {service_id:'b',service_name:'api',wave:1,status:'Queued'}]";
    page.evaluate(format!(
        "Alpine.store('app').deployPlan = {{kind:'service',id:'b',fromDetail:false,items:{items}}}"
    ))
    .await
    .expect("abre o plano de serviço");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let text = eval_str(
        &page,
        "document.querySelector('[x-win=\"\\'deploy_plan\\'\"]')?.innerText || ''",
    )
    .await;
    assert!(
        text.contains("1. db") && text.contains("2. api"),
        "ordem do plano: {text}"
    );
    assert!(
        text.contains("Only the service"),
        "botão só o serviço: {text}"
    );

    page.evaluate(format!(
        "Alpine.store('app').deployPlan = {{kind:'project',id:'p',fromDetail:false,items:{items}}}"
    ))
    .await
    .expect("abre o plano de projeto");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let text = eval_str(
        &page,
        "document.querySelector('[x-win=\"\\'deploy_plan\\'\"]')?.innerText || ''",
    )
    .await;
    assert!(text.contains("Accept"), "botão aceitar: {text}");
    assert!(
        !text.contains("Only the service"),
        "projeto não oferece só o serviço: {text}"
    );

    page.evaluate("Alpine.store('app').closeDeployPlan()")
        .await
        .expect("rejeita");
    assert_eq!(
        eval_str(&page, "String(Alpine.store('app').deployPlan)").await,
        "null"
    );
    assert_eq!(errors(&page).await, Vec::<String>::new(), "sem erros de JS");
}

/// Deploy Engine → Executando → "Detalhes": o clique de verdade abre o modal com os passos e o
/// histórico do deploy, e "Fechar" o esconde (o mesmo botão não abria nada no app desktop).
#[tokio::test]
async fn deploy_engine_details_button_opens_and_closes() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            s.snap.engine = { active: [{ deployment_id: 'dep_1', service_id: 'svc_lento', service_name: 'lento',
                project_name: 'detalhes', kind: 'Deploy', state: 'HealthcheckPolling', elapsed_secs: 25,
                current_state_secs: 20, started_at: '2026-11-22T19:00:00Z',
                states: [{ at: '2026-11-22T19:00:01Z', from: 'Pending', to: 'PreDeployCheck', message: null },
                         { at: '2026-11-22T19:00:05Z', from: 'Staging', to: 'HealthcheckPolling', message: 'esperando a porta' }] }],
                queued: [], recent: [], paused: false, successful_24h: 0, failed_24h: 0, total_24h: 1, uptime_secs: 60 };
            s.view = 'deploy_engine';
        })()"#,
    )
    .await
    .expect("deploy em andamento");
    let result = eval_str(
        &page,
        "(async () => { [...document.querySelectorAll('[x-data=\"deployEngine\"] button')].find((b) => b.textContent.trim().startsWith('Running')).click(); await new Promise((r) => setTimeout(r, 100)); const button = [...document.querySelectorAll('[x-data=\"deployEngine\"] button')].find((b) => b.offsetParent !== null && b.textContent.trim() === 'Details'); if (!button) return 'sem botão: ' + JSON.stringify([...document.querySelectorAll('[x-data=\"deployEngine\"] button')].map((b) => [b.textContent.trim(), b.offsetParent !== null])); button.click(); await new Promise((r) => setTimeout(r, 100)); const box = document.querySelector('[x-win=\"\\'deploy_detail\\'\"]'); const shown = () => { const backdrop = box.closest('.modal__backdrop'); return getComputedStyle(backdrop).display !== 'none' && box.getClientRects().length > 0; }; const opened = shown() ? box.innerText : ''; box.querySelector('.win__dot--close').click(); await new Promise((r) => setTimeout(r, 100)); return JSON.stringify({ title: opened.includes('lento'), message: opened.includes('esperando a porta'), closed: !shown() }); })()",
    )
    .await;
    assert_eq!(result, r#"{"title":true,"message":true,"closed":true}"#);
    assert_eq!(errors(&page).await, Vec::<String>::new(), "sem erros de JS");
}

/// Modal "reordenar serviços": mover (↑/↓ e arrastar) muda só a ordem em edição;
/// Cancelar descarta e o RPC vai com os ids na nova ordem.
#[tokio::test]
async fn services_reorder_modal_moves_and_cancels() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let seed = r#"(() => {
        const s = Alpine.store('app');
        s.snap = { ...(s.snap || {}), services: [['a','alfa'], ['b','beta'], ['c','gama']].map(([k, n]) => ({ service: { id: 'svc_' + k, status: 'Running', spec: { name: n, project_id: 'prj_1' } } })) };
        s.selectedProjectId = 'prj_1';
        s.openServicesReorder();
        s.servicesReorderStep('svc_c', -1);          // a b c -> a c b
        s.servicesReorderMove('svc_b', 'svc_a');     // a c b -> b a c
        return s.servicesReorder.ids.join(',');
    })()"#;
    assert_eq!(
        eval_str(&page, seed).await,
        "svc_b,svc_a,svc_c",
        "ordem em edição"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let text = eval_str(
        &page,
        "document.querySelector('[x-win=\"\\'services_reorder\\'\"]')?.innerText || ''",
    )
    .await;
    assert!(
        text.contains("Reorder") && text.contains("Cancel"),
        "botões: {text}"
    );
    assert!(
        text.find("beta") < text.find("alfa") && text.find("alfa") < text.find("gama"),
        "linhas na ordem editada: {text}"
    );

    page.evaluate("Alpine.store('app').closeServicesReorder()")
        .await
        .expect("cancela");
    assert_eq!(
        eval_str(&page, "String(Alpine.store('app').servicesReorder)").await,
        "null"
    );
    assert_eq!(errors(&page).await, Vec::<String>::new(), "sem erros de JS");
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

/// Renomear serviço pelo título do cabeçalho (como na GUI,
/// `service_title_change`): o input do título começa com o nome, o aviso muda
/// conforme o tipo (Compose × Application), `titleSave()` manda o spec inteiro
/// só com o nome trocado e aparado, não manda nada se o nome não mudou ou
/// ficou vazio, digitar grava sozinho depois do debounce, e o card
/// "Internal URL" de um Compose continua apontando para a chave do YAML
/// depois do rename (o hostname interno não acompanha o nome).
#[tokio::test]
async fn rename_service_in_title() {
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
                spec: { name: 'banco', port: 5432, replicas: 1, project_id: 'prj_1', product: 'postgres',
                    env_vars: [], domains: [],
                    source: { Compose: { content: 'services:\n  # comentário\n\n  rp_banco:\n    image: postgres:18\n' } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
            };
            // Captura o que o título manda gravar, sem daemon.
            window.__saved = [];
            s.api.rpcChecked = async (cmd) => { window.__saved.push(cmd); return { ok: true }; };
            s.refreshNow = async () => {};
            window.__oks = [];
            s.toastOk = (m) => window.__oks.push(m);
        })()"#,
    )
    .await
    .expect("abre o serviço Compose");

    assert!(
        visible(
            &page,
            "[x-data=\"serviceDetail\"] input.service__title[x-model=\"titleDraft\"]"
        )
        .await,
        "o cabeçalho deveria mostrar o título editável"
    );
    let d = "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]'))";
    assert_eq!(
        eval_str(&page, &format!("(() => {d}.titleDraft)()")).await,
        "banco",
        "o título começa com o nome atual"
    );
    assert!(
        eval_str(&page, &format!("(() => {d}.renameNote)()"))
            .await
            .contains("volumes"),
        "Compose: o aviso diz que a stack e os volumes não mudam"
    );

    // Mesmo nome e nome vazio: nada é gravado.
    let _ = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; d.titleDraft = 'banco'; await d.titleSave(); d.titleDraft = '   '; await d.titleSave(); return ''; }})()")
    )
    .await;
    assert_eq!(
        eval_str(&page, "(() => String(window.__saved.length))()").await,
        "0",
        "mesmo nome e vazio não gravam"
    );

    // Nome novo: manda o spec inteiro (fonte, porta, product) só com o nome trocado.
    let saved = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; d.titleDraft = '  meu-banco  '; await d.titleSave(); const x = window.__saved[0].ServiceUpdate; return JSON.stringify({{ n: window.__saved.length, id: x.id, name: x.spec.name, port: x.spec.port, kind: x.spec.product, content: x.spec.source.Compose.content, detail: Alpine.store('app').serviceDetail.spec.name }}); }})()")
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(v["n"], 1);
    assert_eq!(v["id"], "svc_1");
    assert_eq!(v["name"], "meu-banco", "o nome é aparado");
    assert_eq!(v["port"], 5432);
    assert_eq!(v["kind"], "postgres");
    assert!(v["content"].as_str().unwrap().contains("rp_banco:"));
    assert_eq!(
        v["detail"], "meu-banco",
        "o detalhe local passa a ter o nome novo"
    );

    // Digitar grava sozinho, 0,5 s depois da última tecla (sem botão).
    let _ = eval_str(
        &page,
        &format!(
            "(() => {{ const d = {d}; d.titleDraft = 'banco-2'; d.titleInput(); return ''; }})()"
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        eval_str(&page, "(() => window.__saved.length + ':' + window.__saved[window.__saved.length - 1].ServiceUpdate.spec.name)()").await,
        "2:banco-2",
        "o debounce grava o nome digitado"
    );

    // O host interno segue o YAML.
    let url = eval_str(
        &page,
        &format!("(() => {{ {d}.connectionUrl = 'postgresql://u:p@rp_banco:5432/db'; return {d}.connectionInfo.internalUrl; }})()")
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
    assert!(a["note"].as_str().unwrap().contains("next deploy"));
    assert_eq!(a["url"], "—", "sem a URL do daemon, não inventa uma");

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Descrição do serviço: é o campo sem borda do cabeçalho, grava sozinha 0,5 s
/// depois da última tecla (só `ServiceDescriptionSet`, aparado), sem mudança não
/// manda nada, esvaziar grava direto (sem diálogo), e a primeira linha aparece
/// no card do projeto.
#[tokio::test]
async fn service_description_autosaves_in_the_header() {
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
                spec: { name: 'worker', port: 80, replicas: 1, project_id: 'prj_1',
                    description: 'Processa a fila de e-mails.\nSe cair, os e-mails esperam.',
                    env_vars: [], domains: [], source: { Registry: { image: 'worker:1' } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
            };
            window.__saved = [];
            s.api.rpcChecked = async (cmd) => {
                window.__saved.push(cmd);
                const text = cmd.ServiceDescriptionSet.description;
                return { ok: true, value: { Service: { id: 'svc_1', spec: { name: 'worker', description: text || null } } } };
            };
            s.fetchServiceDetail = async () => {};
            s.refreshNow = async () => {};
        })()"#,
    )
    .await
    .expect("abre o serviço com descrição");

    assert!(
        visible(
            &page,
            "[x-data=\"serviceDetail\"] textarea.page-head__subtitle-textarea"
        )
        .await,
        "o cabeçalho do serviço mostra o campo da descrição"
    );
    let d = "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]'))";
    assert_eq!(
        eval_str(&page, &format!("(() => {d}.descriptionText)()")).await,
        "Processa a fila de e-mails.\nSe cair, os e-mails esperam.",
        "o campo começa com o texto gravado"
    );

    // A tecla não grava na hora: é o debounce (0,5 s) que dispara a escrita.
    let digitado = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; d.descriptionText = '  Fila de e-mails.  '; d.descriptionInput(); await new Promise((r) => setTimeout(r, 60)); return JSON.stringify({{ enviados: window.__saved.length, pendente: d.descriptionPending }}); }})()"),
    )
    .await;
    let j: serde_json::Value = serde_json::from_str(&digitado).unwrap();
    assert_eq!(j["enviados"], 0, "a última tecla ainda não gravou");
    assert_eq!(j["pendente"], true, "há rascunho pendente");

    // Passado o debounce: só ServiceDescriptionSet, aparado.
    let aguarda = eval_str(
        &page,
        "(async () => { await new Promise((r) => setTimeout(r, 700)); return JSON.stringify(window.__saved[0] || null); })()",
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&aguarda).unwrap();
    assert_eq!(v["ServiceDescriptionSet"]["service_id"], "svc_1");
    assert_eq!(
        v["ServiceDescriptionSet"]["description"], "Fila de e-mails.",
        "grava aparado, depois do debounce"
    );

    // Sem mudança: nada é gravado.
    let _ = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; d.descriptionText = 'Fila de e-mails.'; d.descriptionInput(); await new Promise((r) => setTimeout(r, 700)); return ''; }})()"),
    )
    .await;
    assert_eq!(
        eval_str(&page, "(() => String(window.__saved.length))()").await,
        "1",
        "texto igual ao gravado não manda nada"
    );

    // Esvaziar grava direto, sem diálogo de confirmação.
    let limpo = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; d.descriptionText = '   '; d.descriptionInput(); await new Promise((r) => setTimeout(r, 700)); return window.__saved.length + ':' + JSON.stringify(window.__saved[1].ServiceDescriptionSet.description); }})()"),
    )
    .await;
    assert_eq!(limpo, "2:\"\"", "esvaziar grava o vazio sem perguntar");

    // O painel da aba General saiu com os botões.
    assert_eq!(
        eval_str(
            &page,
            "(() => document.body.innerText.includes('Salvar descrição') ? 'achou' : 'sumiu')()"
        )
        .await,
        "sumiu"
    );

    // O card do projeto mostra a primeira linha.
    let card = eval_str(
        &page,
        r#"(() => {
            const s = Alpine.store('app');
            s.selectedProjectId = 'prj_1';
            s.snap = Object.assign({}, s.snap || {}, { services: [{ project_name: 'Loja', service: {
                id: 'svc_1', status: 'Running', containers: [],
                spec: { name: 'worker', port: 80, project_id: 'prj_1', description: '\n  Processa a fila.  \nmais',
                    source: { Registry: { image: 'w' } } } } }] });
            const el = document.querySelector('[x-data="projectDetail"]');
            if (!el) return 'sem projectDetail';
            const list = Alpine.$data(el).services;
            return list.length ? list[0].description : 'sem serviços';
        })()"#,
    )
    .await;
    assert_eq!(card, "Processa a fila.");

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Cartão de serviço: a primeira linha cortada da descrição abre um popover com
/// o texto inteiro, como no cartão de projeto, e fecha no clique fora.
#[tokio::test]
async fn service_card_description_opens_popover() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            s.snap = Object.assign({}, s.snap || {}, { services: [{ project_name: 'Loja', service: {
                id: 'svc_1', status: 'Running', containers: [],
                spec: { name: 'worker', port: 80, project_id: 'prj_1',
                    description: 'Processa a fila de e-mails.\nSe cair, os e-mails esperam no broker.',
                    env_vars: [], domains: [], source: { Registry: { image: 'worker:1' } } } } }] });
            s.selectedProjectId = 'prj_1';
            s.view = 'project';
        })()"#,
    )
    .await
    .expect("serviço com descrição de duas linhas");

    let anchor = "[x-data=\"projectDetail\"] .card__description--truncated";
    assert!(
        visible(&page, anchor).await,
        "a descrição cortada do cartão deveria aparecer"
    );
    assert_eq!(
        eval_str(
            &page,
            &format!("(() => document.querySelector({anchor:?}).textContent)()")
        )
        .await,
        "Processa a fila de e-mails.",
        "o cartão mostra só a primeira linha"
    );
    assert!(
        !visible(&page, "[x-data=\"projectDetail\"] .description-popover").await,
        "o popover nasce fechado"
    );

    page.evaluate(format!("document.querySelector({anchor:?}).click()"))
        .await
        .expect("clica na descrição do cartão");
    assert!(
        visible(&page, "[x-data=\"projectDetail\"] .description-popover").await,
        "clicar na linha cortada abre o popover"
    );
    assert_eq!(
        eval_str(
            &page,
            "(() => document.querySelector('[x-data=\"projectDetail\"] .description-popover__text').textContent)()"
        )
        .await,
        "Processa a fila de e-mails.\nSe cair, os e-mails esperam no broker.",
        "o popover mostra a descrição inteira"
    );

    page.evaluate("document.body.click()")
        .await
        .expect("clica fora");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !visible(&page, "[x-data=\"projectDetail\"] .description-popover").await,
        "clicar fora fecha o popover"
    );

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
        .evaluate(
            "[...document.querySelectorAll('.toast-wrap .toast')].map(e => e.className).join(' ')",
        )
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
                spec: { name: 'pg', port: 5432, replicas: 1, project_id: 'prj_i', product: 'postgres', shared: {},
                    env_vars: [], domains: [],
                    source: { Compose: { content: 'services:\n  rp_pg:\n    image: postgres:18\n' } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
            };
        })()"#,
    )
    .await
    .expect("abre o shared server");

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
                spec: { name: 'banco', port: 5432, replicas: 1, project_id: 'prj_a', product: 'postgres',
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
        "(() => { document.querySelector('[title=\"Job actions\"]').click(); return 'ok'; })()",
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
        items, r#"["▶ Run now","◐ Warm","‖ Pause","🔑 Variables and secrets","✎ Edit","✕ Remove"]"#,
        "job parado e sem execução: sem cancelar nem logs"
    );
    assert!(
        eval_str(
            &page,
            "document.querySelector('.job-actions__status').textContent"
        )
        .await
        .contains("never warmed"),
        "a janela mostra por que o job está frio, traduzido (o daemon manda \"nunca aquecido\")"
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
        "(() => { [...document.querySelectorAll('.job-actions__item')].find(el => el.textContent.includes('Warm')).click(); return 'ok'; })()",
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
        .contains("job is already warm"),
        "job quente: o toast avisa que já estava aquecido"
    );

    // Quente: "Reconstruir (sem cache)" aparece, confirma e manda `force: true`.
    eval_str(
        &page,
        "(() => { window.confirm = () => true; window.__commands = []; [...document.querySelectorAll('.job-actions__item')].find(el => el.textContent.includes('Rebuild')).click(); return 'ok'; })()",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        eval_str(&page, "JSON.stringify(window.__commands)").await,
        r#"[{"JobWarm":{"job_id":"job_1","force":true}}]"#
    );
    assert!(
        eval_str(
            &page,
            "JSON.stringify(Alpine.store('app').toasts.map(t => t.message))"
        )
        .await
        .contains("rebuild started"),
        "o toast confirma que a reconstrução começou"
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
        r#"["■ Cancel run","◐ Warm","↻ Rebuild (no cache)","≡ View logs","‖ Pause","🔑 Variables and secrets","✎ Edit","✕ Remove"]"#
    );

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Settings → Sync → "Baixar infra": o daemon devolve os arquivos do `state_git` (`InfraFiles`) e a
/// webui os entrega num `.zip`. A aba "Infra as Code" não existe mais.
#[tokio::test]
async fn infra_download_zip() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            window.__downloads = [];
            const click = HTMLAnchorElement.prototype.click;
            HTMLAnchorElement.prototype.click = function () {
                if (this.download) { window.__downloads.push({ name: this.download, href: this.href }); return; }
                return click.call(this);
            };
            s.api = { baseUrl: location.origin, token: 't',
                rpcChecked: async (cmd) => cmd === 'InfraDownload'
                    ? { ok: true, value: { InfraFiles: [
                        { path: 'MANIFEST.json', content: '{"schema":1}\n' },
                        { path: 'projects/prj_1/project.yml', content: 'name: loja\n' } ] } }
                    : { ok: false, error: 'stub' },
                rpc: async () => ({ ok: false, error: 'stub' }) };
            s.view = 'settings'; s.settingsTab = 'gitsync';
            return 'ok';
        })()"#,
    )
    .await;
    assert!(
        visible(&page, "[x-data=\"settings\"]").await,
        "Settings visível"
    );
    assert_eq!(
        eval_str(
            &page,
            "String([...document.querySelectorAll('.tabs__tab')].some(b => b.textContent.includes('Infra as Code')))"
        )
        .await,
        "false",
        "a aba Infra as Code saiu"
    );
    assert_eq!(
        eval_str(
            &page,
            "String([...document.querySelectorAll('button')].some(b => b.textContent.trim() === 'Download infra'))"
        )
        .await,
        "true",
        "o botão Baixar infra está na aba Sync"
    );

    let out = eval_str(
        &page,
        r#"(async () => {
            await Alpine.store('app').infraDownload();
            const d = window.__downloads[0];
            const bytes = new Uint8Array(await (await fetch(d.href)).arrayBuffer());
            const text = new TextDecoder('latin1').decode(bytes);
            return JSON.stringify({ name: d.name, pk: text.startsWith('PK'),
                tem_manifest: text.includes('MANIFEST.json'), tem_projeto: text.includes('projects/prj_1/project.yml'),
                tem_conteudo: text.includes('name: loja'), msg: Alpine.store('app').infraDownloadMessage });
        })()"#,
    )
    .await;
    assert_eq!(
        out,
        r##"{"name":"rustploy-infra.zip","pk":true,"tem_manifest":true,"tem_projeto":true,"tem_conteudo":true,"msg":"downloaded as rustploy-infra.zip"}"##
    );

    // Erro do daemon (valor em claro no estado): aparece na mensagem, sem baixar nada.
    let err = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            window.__downloads = [];
            s.api.rpcChecked = async () => ({ ok: false, error: 'há 1 valor(es) em claro' });
            await s.infraDownload();
            return JSON.stringify({ n: window.__downloads.length, msg: s.infraDownloadMessage });
        })()"#,
    )
    .await;
    assert_eq!(err, r##"{"n":0,"msg":"erro: há 1 valor(es) em claro"}"##);

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Aba Logs do serviço: nasce do log guardado (`LogsHistory`), "Carregar anteriores" põe a página
/// mais velha por cima e marca onde o container mudou; sem histórico, cai no `LogsGet` ao vivo.
#[tokio::test]
async fn service_logs_start_from_history_and_load_older() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let out = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            const calls = [];
            const e = (t, c, line, stream = 'Stdout') => ({ timestamp: t, container_id: c, stream, line });
            s.api = { baseUrl: location.origin, token: 't',
                rpcChecked: async (cmd) => {
                    const a = cmd.LogsHistory;
                    calls.push(['older', a.before]);
                    return { ok: true, value: { LogsHistory: {
                        entries: [e('2026-11-05T01:00:00Z', 'aaaaaaaaaaaa1234', 'velho 1'), e('2026-11-05T01:00:01Z', 'aaaaaaaaaaaa1234', 'velho 2')],
                        next_before: null } } };
                },
                rpc: async (cmd) => {
                    if (cmd.LogsHistory) {
                        calls.push(['seed', cmd.LogsHistory.before ?? null]);
                        return { ok: true, value: { LogsHistory: {
                            entries: [e('2026-11-05T02:00:00Z', 'bbbbbbbbbbbb9999', 'novo 1'), e('2026-11-05T02:00:01Z', 'bbbbbbbbbbbb9999', 'novo erro', 'Stderr')],
                            next_before: '2026-11-05T02:00:00Z' } } };
                    }
                    return { ok: false, error: 'stub' };
                } };
            s.selectedServiceId = 'svc_1'; s.serviceTab = 'logs';
            await s.startServiceLogs();
            const seeded = s.serviceLogLines.map(l => l.line);
            const more1 = s.serviceLogMore;
            await s.serviceLogLoadOlder();
            return JSON.stringify({ seeded, more1, after: s.serviceLogLines.map(l => l.line),
                more2: s.serviceLogMore, calls });
        })()"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["seeded"], serde_json::json!(["novo 1", "novo erro"]));
    assert_eq!(v["more1"], "2026-11-05T02:00:00Z", "há mais log guardado");
    assert_eq!(
        v["after"],
        serde_json::json!([
            "velho 1",
            "velho 2",
            "— other container: bbbbbbbbbbbb —",
            "novo 1",
            "novo erro"
        ]),
        "o mais velho vem por cima, com separador onde o container mudou"
    );
    assert_eq!(v["more2"], serde_json::Value::Null, "acabou o histórico");
    assert_eq!(
        v["calls"][1],
        serde_json::json!(["older", "2026-11-05T02:00:00Z"])
    );

    // Sem histórico (Compose, log desligado): cai no log ao vivo do motor.
    let fallback = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            s.api = { baseUrl: location.origin, token: 't',
                rpcChecked: async () => ({ ok: false, error: 'stub' }),
                rpc: async (cmd) => cmd.LogsGet
                    ? { ok: true, value: { Logs: [{ stream: 'Stdout', line: 'ao vivo', timestamp: '2026-11-05T03:00:00Z' }] } }
                    : { ok: false, error: 'LogsDisabled' } };
            s.serviceLogLines = [];
            await s.startServiceLogs();
            return JSON.stringify({ lines: s.serviceLogLines.map(l => l.line), more: s.serviceLogMore });
        })()"#,
    )
    .await;
    assert_eq!(fallback, r##"{"lines":["ao vivo"],"more":null}"##);
    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Aba Logs por execução: o painel ao vivo mostra SÓ a execução atual; as anteriores viram uma lista
/// (deploy, imagem, desfecho) com "Ver logs" (janela só dela, com data e hora), excluir e
/// "limpar selecionados" (em lote, com confirmação); depois de excluir a lista recarrega.
#[tokio::test]
async fn service_log_executions_list_open_and_delete() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let out = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            window.confirm = () => true;
            const calls = [];
            let executions = [
              { container_ids: ['cur000000001'], deployment_id: 'dep_3', image: 'api:3', started_at: '2026-11-06T10:00:00Z', ended_at: '2026-11-06T11:00:00Z', lines: 30, bytes: 3000, current: true, partial: false, outcome: 'Live' },
              { container_ids: ['old000000002'], deployment_id: 'dep_2', image: 'api:2', started_at: '2026-11-05T10:00:00Z', ended_at: '2026-11-05T11:00:00Z', lines: 20, bytes: 2000, current: false, partial: false, outcome: 'Stopped' },
              { container_ids: ['bad000000003'], deployment_id: 'dep_1', image: 'api:1', started_at: '2026-11-04T10:00:00Z', ended_at: '2026-11-04T10:05:00Z', lines: 5, bytes: 500, current: false, partial: true, outcome: 'Failed' },
            ];
            const entry = (c, line) => ({ timestamp: '2026-11-05T10:00:00Z', container_id: c, stream: 'Stdout', line });
            s.api = { baseUrl: location.origin, token: 't',
                rpcChecked: async (cmd) => {
                    if (cmd.LogsHistory) {
                        calls.push(['history', cmd.LogsHistory.container_ids]);
                        return { ok: true, value: { LogsHistory: { entries: [entry('old000000002', 'da execução velha')], next_before: null } } };
                    }
                    if (cmd.LogsExecutionDelete) {
                        calls.push(['delete', cmd.LogsExecutionDelete.container_ids]);
                        executions = executions.filter(e => !cmd.LogsExecutionDelete.container_ids.includes(e.container_ids[0]));
                        return { ok: true, value: { LogsExecutionsDeleted: { deleted: cmd.LogsExecutionDelete.container_ids.length, freed_bytes: 2500 } } };
                    }
                    return { ok: false, error: 'stub' };
                },
                rpc: async (cmd) => {
                    if (cmd.LogsExecutions) return { ok: true, value: { LogsExecutions: executions } };
                    if (cmd.LogsHistory) {
                        calls.push(['live', cmd.LogsHistory.container_ids]);
                        return { ok: true, value: { LogsHistory: { entries: [entry('cur000000001', 'ao vivo atual')], next_before: null } } };
                    }
                    return { ok: false, error: 'stub' };
                } };
            s.selectedServiceId = 'svc_1'; s.serviceTab = 'logs';
            await s.startServiceLogs();
            const live = s.serviceLogLines.map(l => l.line);
            const past = s.serviceLogExecutions.map(r => [r.key, r.outcomeLabel, r.partial]);
            await s.serviceLogExecutionOpen(s.serviceLogExecutions[0]);
            const win = { title: s.serviceLogExecWin.title, lines: s.serviceLogExecWin.lines.map(l => l.line) };
            s.serviceLogToggleAll();
            const selected = [...s.serviceLogSelected];
            await s.serviceLogSelectedDelete();
            return JSON.stringify({ live, past, win, selected, calls,
                after: s.serviceLogExecutions.length, sel_after: s.serviceLogSelected.length });
        })()"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["live"], serde_json::json!(["ao vivo atual"]));
    assert_eq!(
        v["calls"][0],
        serde_json::json!(["live", ["cur000000001"]]),
        "o painel ao vivo pede só a execução atual"
    );
    assert_eq!(
        v["past"],
        serde_json::json!([
            ["old000000002", "Replaced", false],
            ["bad000000003", "Failed", true]
        ]),
        "a atual não entra na lista; desfecho e parcial aparecem"
    );
    assert_eq!(v["win"]["lines"], serde_json::json!(["da execução velha"]));
    assert!(
        v["win"]["title"].as_str().unwrap().contains("→"),
        "o título diz qual execução é"
    );
    assert_eq!(
        v["calls"][1],
        serde_json::json!(["history", ["old000000002"]])
    );
    assert_eq!(
        v["selected"],
        serde_json::json!(["old000000002", "bad000000003"])
    );
    assert_eq!(
        v["calls"][2],
        serde_json::json!(["delete", ["old000000002", "bad000000003"]]),
        "limpar selecionados = uma chamada em lote"
    );
    assert_eq!(v["after"], 0, "a lista recarrega depois de excluir");
    assert_eq!(v["sel_after"], 0);
    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Tela "Em execução": só o que `snapshot.dashboard` traz, com o webhook mascarado até
/// "Mostrar", os pré-deploy checks na ordem e os atalhos de abrir/deploy/parar.
#[tokio::test]
async fn running_screen_lists_running_services() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let seed = r#"(() => {
        const s = Alpine.store('app');
        s.snap.dashboard = [{ project_id: 'prj_1', project_name: 'acme', services: [{
            service_id: 'svc_1', name: 'api', status: 'Running', kind: 'git', kind_label: 'App · Git',
            branch: 'release', repo_url: 'https://github.com/acme/api.git', image: null, engine: null,
            port: 8080, domains: ['api.acme.dev'], webhook: 'https://h/webhook/svc_1/segredo',
            pre_deploy_checks: [{ job_id: 'j1', name: 'migrate' }, { job_id: 'j2', name: 'seed' }],
            warm: { service_id: 'svc_1', warm: false, warming: false, reason: 'nunca aquecido', warmed_at: null } }] }];
        s.view = 'running';
        return 'ok';
    })()"#;
    assert!(
        !eval_str(&page, &format!("(() => {seed})()"))
            .await
            .is_empty()
    );
    assert!(
        visible(&page, "[x-data=\"running\"]").await,
        "tela running visível"
    );

    let text = eval_str(
        &page,
        "document.querySelector('[x-data=\"running\"]').innerText",
    )
    .await;
    for esperado in [
        "acme",
        "api",
        "release",
        "migrate",
        "seed",
        "Deploy",
        "Open service",
        "Open project",
    ] {
        assert!(
            text.contains(esperado),
            "faltou {esperado:?} na tela: {text}"
        );
    }
    // A URL fica no `value` do input (não no innerText): mascarada até "Mostrar".
    let url = eval_str(
        &page,
        r#"document.querySelector('[x-data="running"] .webhook__url').value"#,
    )
    .await;
    assert_eq!(url, "https://h/webhook/svc_1/••••••••");
    assert!(
        !text.contains("segredo"),
        "o token do webhook não pode aparecer: {text}"
    );
    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// O selo de versão nova acende pelo snapshot, sem ninguém clicar em "Verificar": o campo `update`
/// traz a última consulta do daemon (inclusive a periódica). Um snapshot com o mesmo `checked_at`
/// não reescreve a aba; um com `checked_at` novo, sim.
#[tokio::test]
async fn update_badge_lights_up_from_the_snapshot() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;
    assert!(
        !visible(&page, ".nav-item__badge").await,
        "sem `update` no snapshot o selo fica apagado"
    );

    let applied = eval_str(
        &page,
        &format!(
            r#"(() => {{
                const s = Alpine.store('app');
                const snapshot = {SNAPSHOT_JS};
                snapshot.update = {{ current_version: '0.1.14', latest_version: '0.1.15',
                    is_update_available: true, release_url: 'https://github.com/o/r/releases/tag/v0.1.21',
                    published_at: null, checked_at: '2026-10-27T10:00:00Z' }};
                s.applySnapshot(snapshot);
                return JSON.stringify([s.updateIsAvailable, s.updateLatestVersion, s.updateMessage]);
            }})()"#
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&applied).unwrap();
    assert_eq!(v[0], true);
    assert_eq!(v[1], "0.1.15");
    assert_eq!(v[2], "There is a new version: 0.1.15");
    assert!(
        visible(&page, ".nav-item__badge").await,
        "o selo do Settings deveria acender"
    );

    let after = eval_str(
        &page,
        &format!(
            r#"(() => {{
                const s = Alpine.store('app');
                const snapshot = {SNAPSHOT_JS};
                snapshot.update = {{ current_version: '0.1.14', latest_version: '0.1.15',
                    is_update_available: true, release_url: '', published_at: null,
                    checked_at: '2026-10-27T10:00:00Z' }};
                s.updateMessage = 'mensagem da tela';
                s.applySnapshot(snapshot);
                const kept = s.updateMessage;
                snapshot.update = {{ ...snapshot.update, latest_version: '0.1.14',
                    is_update_available: false, checked_at: '2026-10-27T16:00:00Z' }};
                s.applySnapshot(snapshot);
                return JSON.stringify([kept, s.updateIsAvailable]);
            }})()"#
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        v[0], "mensagem da tela",
        "mesmo checked_at: a aba não é reescrita"
    );
    assert_eq!(v[1], false, "checked_at novo: a aba segue a consulta nova");

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Abrir Settings já carrega a lista de contas Git (como o botão "Refresh git list"), para a aba
/// Sync não ficar vazia quando a pessoa não passou pela aba Git antes.
#[tokio::test]
async fn opening_settings_loads_the_git_provider_list() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let out = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            const calls = [];
            s.api = { rpc: async (c) => {
                calls.push(typeof c === 'string' ? c : Object.keys(c)[0]);
                return { ok: true, value: { GitProviders: [{ id: 'gp_1', name: 'GitHub', kind: 'Github' }] } };
            } };
            s.gitProviders = [];
            s.nav('settings');
            await new Promise(r => setTimeout(r, 100));
            return JSON.stringify([calls.includes('GitProviderList'), s.gitProviders.length]);
        })()"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0], true, "nav('settings') pede GitProviderList");
    assert_eq!(v[1], 1, "a lista chega ao estado");
}

/// A sincronização do estado com o git (Fases 6 e 7 do plano de estado em git): o aviso do topo, a
/// aba "Sync" do Settings (situação, divergência, commit recusado, removidos, auditoria), o selo
/// "mudança pendente de deploy" e o container órfão em Runtime → Containers. Tudo vem do snapshot.
#[tokio::test]
async fn git_sync_screens_render_from_the_snapshot() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    // Sem `state_git` no snapshot (daemon antigo, ou ainda carregando): nada de aviso e nada quebra.
    assert!(
        !eval_str(
            &page,
            "String(Alpine.store('app').stateGitAlertMessage === '')"
        )
        .await
        .is_empty()
    );
    assert_eq!(
        eval_str(&page, "Alpine.store('app').stateGitAlertMessage").await,
        ""
    );

    let seeded = eval_str(
        &page,
        &format!(
            r#"(() => {{
                const s = Alpine.store('app');
                const snapshot = {SNAPSHOT_JS};
                snapshot.state_git = {{
                    mode: 'sync', remote: 'https://github.com/o/estado.git', branch: 'main', provider_id: null,
                    git_available: true, recovery_configured: false,
                    last_commit: {{ sha: 'da3da09aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', at: '2026-11-02T06:28:44Z', subject: 'Atualiza projeto E2E' }},
                    last_push_ok: true, last_push_at: '2026-11-02T06:28:50Z', last_fetch_at: '2026-11-02T06:29:00Z',
                    last_error: null, blocked_by_plaintext: 0, needs_push: false, ahead: 1, behind: 1,
                    paused_reason: 'o histórico divergiu',
                    incoming_rejected: {{ sha: '40c22c1240580bde3cac6512dab507c663335a85', reason: 'traz 1 valor(es) em claro' }},
                    removed_in_git: [{{ id: 'serviço:svc_web', kind: 'serviço', name: 'web', project: 'E2E',
                                       sha: 'da3da09aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', at: '2026-11-02T06:28:44Z' }}],
                    pending_deploy: ['svc_pendente'],
                }};
                snapshot.infra_audit = [
                    {{ definite: false, allowed: false, project: 'E2E', scope: 'projeto', what: 'env PASSWORD_MIN_LENGTH' }},
                    {{ definite: true, allowed: false, project: 'E2E', scope: 'serviço api', what: 'token de webhook sem cifra' }},
                ];
                snapshot.docker_containers = [
                    {{ id: 'a'.repeat(64), name: 'orfao_vivo', image: 'debian', state: 'running', status: 'Up',
                       managed: true, project: null, service: null, removed_in_git: 'web' }},
                    {{ id: 'b'.repeat(64), name: 'servico_vivo', image: 'debian', state: 'running', status: 'Up',
                       managed: true, project: 'E2E', service: 'api', removed_in_git: null }},
                ];
                s.applySnapshot(snapshot);
                return JSON.stringify([s.stateGitAlertMessage, s.stateGitPendingDeploy('svc_pendente'),
                                       s.stateGitPendingDeploy('outro'), s.stateGitAuditFindings.length]);
            }})()"#
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&seeded).unwrap();
    assert!(
        v[0].as_str().unwrap().contains("paused"),
        "a pausa tem prioridade no aviso: {}",
        v[0]
    );
    assert_eq!(v[1], true, "serviço com mudança pendente de deploy");
    assert_eq!(v[2], false);
    assert_eq!(v[3], 2, "a auditoria vem no snapshot");
    assert!(
        visible(&page, ".banner--warn").await,
        "o aviso do topo aparece (pausada)"
    );

    // Settings → Sync
    let page_text = eval_str(
        &page,
        r#"(() => {
            const s = Alpine.store('app');
            s.nav('settings');
            s.settingsTab = 'gitsync';
            s.stateGitFormLoad();
            return 'ok';
        })()"#,
    )
    .await;
    assert!(!page_text.is_empty());
    // O Alpine aplica `x-show`/`x-if` num microtask: espera a tela de fato aparecer antes de medir
    // (senão `innerText` de um elemento ainda oculto devolve o texto todo e o teste passa em falso).
    assert!(
        visible(&page, "[x-data=\"settings\"]").await,
        "Settings visível"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let text = eval_str(
        &page,
        "document.querySelector('[x-data=\"settings\"]').innerText",
    )
    .await;
    for esperado in [
        "GIT SYNC",
        "DIVERGED HISTORY",
        "Use the repository's",
        "Use the server's",
        "Resume",
        "REPOSITORY COMMIT REJECTED",
        "REMOVED IN GIT",
        "Dismiss",
        "PLAINTEXT VALUES (2)",
        "env PASSWORD_MIN_LENGTH",
        "Not a secret",
        "da3da09 · Atualiza projeto E2E",
    ] {
        assert!(
            text.contains(esperado),
            "faltou {esperado:?} na aba Sync:\n{text}"
        );
    }
    // O formulário parte do status gravado no daemon.
    assert_eq!(
        eval_str(&page, "Alpine.store('app').stateGitRemote").await,
        "https://github.com/o/estado.git"
    );
    // A certeza (token de webhook) NÃO pode ser liberada: só a suspeita tem o botão.
    let buttons = eval_str(
        &page,
        r#"JSON.stringify([...document.querySelectorAll('[x-data="settings"] button')]
            .filter(b => b.getClientRects().length > 0).map(b => b.innerText.trim()))"#,
    )
    .await;
    let buttons: Vec<String> = serde_json::from_str(&buttons).unwrap();
    assert_eq!(
        buttons
            .iter()
            .filter(|b| b.as_str() == "Not a secret")
            .count(),
        1,
        "só a suspeita tem o botão; botões visíveis: {buttons:?}"
    );

    // Runtime → Containers: o órfão que ainda roda oferece "Stop"; o container de serviço vivo, não.
    eval_str(
        &page,
        "(() => { Alpine.store('app').nav('docker'); return 'ok'; })()",
    )
    .await;
    assert!(
        visible(&page, "[x-data=\"docker\"]").await,
        "Runtime visível"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let rows = eval_str(
        &page,
        "document.querySelector('[x-data=\"docker\"]').innerText",
    )
    .await;
    assert!(
        rows.contains("removed in git (web)"),
        "dono do órfão:\n{rows}"
    );
    let docker_buttons = eval_str(
        &page,
        r#"JSON.stringify([...document.querySelectorAll('[x-data="docker"] button')]
            .filter(b => b.getClientRects().length > 0).map(b => b.innerText.trim()))"#,
    )
    .await;
    let docker_buttons: Vec<String> = serde_json::from_str(&docker_buttons).unwrap();
    assert_eq!(
        docker_buttons
            .iter()
            .filter(|b| b.as_str() == "Stop")
            .count(),
        1,
        "só o órfão tem o botão Stop; botões visíveis: {docker_buttons:?}"
    );

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Settings → Backups (docs/arquivo/plano-backup-de-dados-para-s3.md, Fase 1): abrir Settings lê o destino S3
/// gravado, o formulário o recebe **sem** chave secreta, e salvar manda a chave só quando digitada.
#[tokio::test]
async fn backups_tab_loads_and_saves_the_s3_destination() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let out = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            const sent = [];
            const view = { configured: true, endpoint: 'http://rp_s3:9000', region: 'us-east-1', bucket: 'bk',
                           prefix: 'rustploy', path_style: true, access_key: 'AK', secret_set: true,
                           recovery_configured: false };
            s.api = { rpc: async (c) => {
                const name = typeof c === 'string' ? c : Object.keys(c)[0];
                sent.push(c);
                if (name === 'BackupDestinationGet' || name === 'BackupDestinationSet')
                    return { ok: true, value: { BackupDestination: view } };
                return { ok: true, value: 'Ok' };
            } };
            s.api.rpcChecked = s.api.rpc;
            s.nav('settings');
            await new Promise(r => setTimeout(r, 100));
            const loaded = [s.backupDestBucket, s.backupDestAccessKey, s.backupDestSecretSet,
                            s.backupDestRecoveryMissing, s.backupDestSecretKey];
            // salvar sem digitar a chave: manda `secret_key: null` (mantém a gravada)
            await s.backupDestinationSave();
            const semChave = sent.find(c => c.BackupDestinationSet).BackupDestinationSet.destination.secret_key;
            // digitando a chave: ela vai, e some do campo depois
            s.backupDestSecretKey = 'nova-chave';
            await s.backupDestinationSave();
            const sets = sent.filter(c => c.BackupDestinationSet);
            const comChave = sets[1].BackupDestinationSet.destination.secret_key;
            return JSON.stringify({ loaded, semChave, comChave, depois: s.backupDestSecretKey });
        })()"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["loaded"], serde_json::json!(["bk", "AK", true, true, ""]));
    assert_eq!(
        v["semChave"],
        serde_json::Value::Null,
        "sem digitar, mantém a chave gravada"
    );
    assert_eq!(v["comChave"], "nova-chave");
    assert_eq!(v["depois"], "", "a chave digitada não fica no formulário");
}

/// Aba Dados → "Backup no S3" (docs/arquivo/plano-backup-de-dados-para-s3.md, Fase 2): abrir a aba lista os
/// backups e a última operação; "Fazer backup agora" e "Restaurar" mandam o comando certo (restaurar com
/// `confirm: true`); sem destino configurado a tela mostra o motivo e esconde o botão.
#[tokio::test]
async fn data_tab_shows_s3_backups_and_sends_the_commands() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let out = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            s.view = 'service';
            s.selectedServiceId = 'svc_1';
            s.serviceTab = 'dados';
            s.serviceLoading = false;
            s.serviceDeployments = [];
            s.serviceDetail = { id: 'svc_1', status: 'Running', live_container_id: 'x',
                spec: { name: 'db', port: 5432, replicas: 1, project_id: 'prj_1', env_vars: [], domains: [],
                    source: { Registry: { image: 'busybox' } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } } };
            const sent = [];
            let blocked = null;
            const lista = () => ({ ServiceBackups: {
                entries: [{ key: 'p/prj_1/svc_1/20261111-100000.rpdata.age', last_modified: '2026-11-11T10:00:00Z', size_bytes: 3008554 }],
                last_run: { op: 'backup', state: 'ok', started_at: 'a', finished_at: '2026-11-11T10:00:05Z', size_bytes: 3008554, key: 'k', error: null, checks: [] },
                blocked, list_error: null } });
            const rpc = async (c) => {
                const name = typeof c === 'string' ? c : Object.keys(c)[0];
                sent.push(c);
                if (name === 'ServiceDataPlan') return { ok: true, value: { ServiceDataPlan: { service_name: 'db', parts: [], running: true, blocked: null, notes: [], encrypt_default: true } } };
                if (name === 'ServiceBackupList') return { ok: true, value: lista() };
                return { ok: true, value: 'Ok' };
            };
            s.api = { rpc, rpcChecked: rpc };
            const el = document.querySelector('[x-data="serviceDetail"]');
            const d = Alpine.$data(el);
            await d.loadData();
            await new Promise(r => setTimeout(r, 100));
            const texto = el.innerText;
            const listado = [d.backups.entries.length, d.backupRunLabel.includes('3 MB'), texto.includes('Fazer backup agora') || texto.includes('Back up now')];
            // restaurar manda confirm:true (o confirm do navegador é aceito aqui)
            window.confirm = () => true;
            await d.backupNow();
            await d.restoreBackup(d.backups.entries[0]);
            const restore = sent.find(c => c.ServiceBackupRestore).ServiceBackupRestore;
            const now = sent.find(c => c.ServiceBackupNow).ServiceBackupNow;
            // sem destino: o motivo aparece e o botão some
            blocked = 'nenhum destino S3 configurado';
            await d.loadBackups();
            await new Promise(r => setTimeout(r, 100));
            const semDestino = [el.innerText.includes('nenhum destino S3 configurado'),
                !!Array.from(el.querySelectorAll('button')).find(b => /Fazer backup agora|Back up now/.test(b.innerText) && b.offsetParent !== null)];
            return JSON.stringify({ listado, restore, now, semDestino });
        })()"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["listado"], serde_json::json!([1, true, true]), "{out}");
    assert_eq!(v["restore"]["confirm"], true, "{out}");
    assert_eq!(v["restore"]["service_id"], "svc_1", "{out}");
    assert_eq!(v["now"]["service_id"], "svc_1", "{out}");
    assert_eq!(v["semDestino"], serde_json::json!([true, false]), "{out}");
}

/// Aba Dados → política do backup automático (Fase 3): o formulário nasce da política gravada (semanal,
/// 04:30 UTC), editar e salvar manda o `Recurrence` certo para as três frequências, e o repoll não apaga
/// o que a pessoa está digitando.
#[tokio::test]
async fn data_tab_backup_policy_form_roundtrips() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let out = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            s.view = 'service'; s.selectedServiceId = 'svc_1'; s.serviceTab = 'dados';
            s.serviceLoading = false; s.serviceDeployments = [];
            s.serviceDetail = { id: 'svc_1', status: 'Running', live_container_id: 'x',
                spec: { name: 'db', port: 5432, replicas: 1, project_id: 'prj_1', env_vars: [], domains: [],
                    source: { Registry: { image: 'busybox' } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } } };
            const sent = [];
            const policy = { enabled: true, recurrence: { Weekly: { weekday: 2, hour: 4, minute: 30 } }, keep: 5, stop: false };
            const lista = (p) => ({ ServiceBackups: { entries: [], last_run: null, blocked: null, list_error: null,
                policy: p, next_run_at: p && p.enabled ? '2026-11-12T04:30:00+00:00' : null } });
            const rpc = async (c) => {
                const name = typeof c === 'string' ? c : Object.keys(c)[0];
                sent.push(c);
                if (name === 'ServiceDataPlan') return { ok: true, value: { ServiceDataPlan: { service_name: 'db', parts: [], running: false, blocked: null, notes: [], encrypt_default: true } } };
                if (name === 'ServiceBackupList') return { ok: true, value: lista(policy) };
                if (name === 'ServiceBackupPolicySet') return { ok: true, value: lista(c.ServiceBackupPolicySet.policy) };
                return { ok: true, value: 'Ok' };
            };
            s.api = { rpc, rpcChecked: rpc };
            const el = document.querySelector('[x-data="serviceDetail"]');
            const d = Alpine.$data(el);
            await d.loadData();
            await new Promise(r => setTimeout(r, 100));
            const f = d.backupForm;
            const lido = [f.enabled, f.kind, f.weekday, f.time, f.keep, f.stop];
            // o repoll (loadBackups sem preencher) não apaga uma edição em andamento
            d.backupForm.keep = 9;
            await d.loadBackups(false);
            const mantido = d.backupForm.keep;
            const enviar = async (patch) => {
                Object.assign(d.backupForm, patch);
                await d.saveBackupPolicy();
                return sent.filter(c => c.ServiceBackupPolicySet).pop().ServiceBackupPolicySet.policy;
            };
            const diario = await enviar({ kind: 'daily', time: '03:15', keep: 7, stop: true });
            const horas = await enviar({ kind: 'interval', hours: 6 });
            const semanal = await enviar({ kind: 'weekly', weekday: 6, time: '23:00' });
            const desligado = await enviar({ enabled: false });
            return JSON.stringify({ lido, mantido, diario, horas, semanal, desligado: desligado.enabled,
                proximo: el.innerText.includes('2026-11-12') });
        })()"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["lido"],
        serde_json::json!([true, "weekly", 2, "04:30", 5, false]),
        "{out}"
    );
    assert_eq!(v["mantido"], 9, "{out}");
    assert_eq!(
        v["diario"]["recurrence"],
        serde_json::json!({"Daily": {"hour": 3, "minute": 15}}),
        "{out}"
    );
    assert_eq!(v["diario"]["keep"], 7);
    assert_eq!(
        v["horas"]["recurrence"],
        serde_json::json!({"IntervalHours": 6}),
        "{out}"
    );
    assert_eq!(
        v["semanal"]["recurrence"],
        serde_json::json!({"Weekly": {"weekday": 6, "hour": 23, "minute": 0}}),
        "{out}"
    );
    assert_eq!(v["desligado"], false, "{out}");
}

/// Arquivo de recuperação (`.rpkit`): "Gerar" manda repositório, branch e conta junto, baixa o binário que
/// veio em base64, e a aba Recuperar, ao carregar esse arquivo, preenche os quatro campos; um arquivo com
/// um byte trocado é recusado com mensagem. Plano: docs/arquivo/plano-arquivo-de-recuperacao.md.
#[tokio::test]
async fn recovery_kit_generate_sends_the_fields_and_recover_loads_the_file() {
    use base64::Engine as _;
    let kit = crate::features::data_backup::recovery_kit::RecoveryKit {
        infra_remote: "https://github.com/org/infra.git".into(),
        infra_branch: "estado".into(),
        infra_token: "ghp_do_arquivo".into(),
        private_key: "AGE-SECRET-KEY-1DOARQUIVO".into(),
        public_key: "age1doarquivo".into(),
        server: "vps-teste".into(),
        created_at: "2026-11-18T12:00:00+00:00".into(),
    };
    let kit_base64 = base64::prelude::BASE64_STANDARD.encode(kit.encode().unwrap());

    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let script = r#"(async () => {
        const s = Alpine.store('app');
        s.view = 'settings'; s.settingsTab = 'recoverykey';
        const sent = [];
        const rpc = async (c) => {
            const name = typeof c === 'string' ? c : Object.keys(c)[0];
            sent.push(c);
            if (name === 'RecoveryKeyStatus') return { ok: true, value: { RecoveryKey: { configured: false, public_key: null } } };
            if (name === 'RecoveryKeyGenerate') return { ok: true, value: { RecoveryKeyGenerated: {
                public_key: 'age1doarquivo', private_key: 'AGE-SECRET-KEY-1DOARQUIVO',
                file_base64: '__KIT__', suggested_filename: 'rustploy-recuperacao-vps.rpkit',
                warning: 'conta OAuth' } } };
            if (name === 'ServerRecoverStatus') return { ok: true, value: { ServerRecover: { phase: 'idle', projects: 0, services: [] } } };
            return { ok: true, value: 'Ok' };
        };
        s.api = { rpc, rpcChecked: rpc };
        let baixado = null;
        s.downloadBytesFile = (nome, bytes) => { baixado = { nome, tamanho: bytes.length, primeiro: bytes[0] }; };
        // o Git Sync configurado pré-preenche o formulário
        s.stateGitRemote = 'https://github.com/org/infra.git'; s.stateGitBranch = 'estado'; s.stateGitProviderId = 'p1';
        await s.recoveryKeyLoad();
        const preenchido = [s.recoveryKeyInfraRemote, s.recoveryKeyInfraBranch, s.recoveryKeyProviderId];
        // sem conta: não vai ao daemon
        s.recoveryKeyProviderId = '';
        await s.recoveryKeyGenerate();
        const semConta = sent.filter(c => c.RecoveryKeyGenerate).length;
        s.recoveryKeyProviderId = 'p1';
        await s.recoveryKeyGenerate();
        const gen = sent.filter(c => c.RecoveryKeyGenerate).pop().RecoveryKeyGenerate;

        // aba Recuperar: carrega o mesmo arquivo
        s.settingsTab = 'recover';
        await s.recoverStatusLoad();
        const bin = atob('__KIT__');
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        await s.recoverLoadKit(new File([bytes], 'x.rpkit'));
        const campos = [s.recoverRemote, s.recoverBranch, s.recoverToken, s.recoverKey];
        const botao = document.body.innerText.includes('Load recovery file') || document.body.innerText.includes('Carregar arquivo de recuperação');
        // um byte trocado: recusado, campos limpos não são sobrescritos
        s.recoverRemote = ''; s.recoverKey = '';
        const ruim = bytes.slice(); ruim[20] ^= 1;
        await s.recoverLoadKit(new File([ruim], 'ruim.rpkit'));
        return JSON.stringify({ preenchido, semConta, gen, baixado, aviso: s.recoveryKeyWarning, campos, botao,
            depoisDoRuim: [s.recoverRemote, s.recoverKey], mensagemRuim: s.recoverInfo.message || '' });
    })()"#
    .replace("__KIT__", &kit_base64);
    let out = eval_str(&page, &script).await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["preenchido"],
        serde_json::json!(["https://github.com/org/infra.git", "estado", "p1"]),
        "{out}"
    );
    assert_eq!(v["semConta"], 0, "sem conta não devia ir ao daemon: {out}");
    assert_eq!(
        v["gen"]["infra_remote"], "https://github.com/org/infra.git",
        "{out}"
    );
    assert_eq!(v["gen"]["infra_branch"], "estado", "{out}");
    assert_eq!(v["gen"]["provider_id"], "p1", "{out}");
    assert_eq!(
        v["baixado"]["nome"], "rustploy-recuperacao-vps.rpkit",
        "{out}"
    );
    assert_eq!(
        v["baixado"]["primeiro"], 0x89,
        "baixou o binário, não o base64: {out}"
    );
    assert_eq!(v["aviso"], "conta OAuth", "{out}");
    assert_eq!(
        v["campos"],
        serde_json::json!([
            "https://github.com/org/infra.git",
            "estado",
            "ghp_do_arquivo",
            "AGE-SECRET-KEY-1DOARQUIVO"
        ]),
        "{out}"
    );
    assert_eq!(v["botao"], true, "botão de carregar ausente: {out}");
    assert_eq!(v["depoisDoRuim"], serde_json::json!(["", ""]), "{out}");
    assert!(
        v["mensagemRuim"].as_str().unwrap_or("").contains("corrupt"),
        "{out}"
    );
}

/// Settings → Recuperar (Fase 4): o formulário manda o repositório, o token e a chave; **a chave e o token
/// somem dos campos** deem certo ou não; o resumo mostra cada serviço com o backup achado; confirmar manda
/// `ServerRecoverRun` e cancelar volta ao começo.
#[tokio::test]
async fn recover_tab_sends_the_fields_and_forgets_the_secrets() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    let out = eval_str(
        &page,
        r#"(async () => {
            const s = Alpine.store('app');
            s.view = 'settings'; s.settingsTab = 'recover';
            const sent = [];
            let fail = true;
            const ready = { phase: 'ready', projects: 1, destination_configured: true, destination_error: null,
                services: [
                  { service_id: 'svc_db', name: 'banco', project: 'loja', kind: 'database', latest_backup: '2026-11-11T13:44:06Z', step: 'pending', error: null },
                  { service_id: 'svc_web', name: 'web', project: 'loja', kind: 'volume', latest_backup: null, step: 'pending', error: null } ] };
            const rpc = async (c) => {
                const name = typeof c === 'string' ? c : Object.keys(c)[0];
                sent.push(c);
                if (name === 'ServerRecoverStatus') return { ok: true, value: { ServerRecover: { phase: 'idle', projects: 0, services: [] } } };
                if (name === 'ServerRecoverStart') return fail ? { ok: false, error: 'token inválido' } : { ok: true, value: { ServerRecover: ready } };
                if (name === 'ServerRecoverCancel') return { ok: true, value: { ServerRecover: { phase: 'idle', projects: 0, services: [], message: 'cancelada' } } };
                return { ok: true, value: 'Ok' };
            };
            s.api = { rpc, rpcChecked: rpc };
            window.confirm = () => true;
            await s.recoverStatusLoad();
            s.recoverRemote = ' https://github.com/org/estado.git ';
            s.recoverToken = 'ghp_segredo';
            s.recoverKey = 'AGE-SECRET-KEY-1ABC';
            // 1ª tentativa falha: mesmo assim os segredos saem dos campos
            await s.recoverStart();
            const aposErro = [s.recoverKey, s.recoverToken, s.recoverRemote.trim()];
            // 2ª dá certo
            fail = false;
            s.recoverKey = 'AGE-SECRET-KEY-1ABC'; s.recoverToken = 'tok';
            await s.recoverStart();
            await new Promise(r => setTimeout(r, 100));
            const start = sent.filter(c => c.ServerRecoverStart).pop().ServerRecoverStart;
            const texto = document.body.innerText;
            const resumo = [s.recoverInfo.phase, texto.includes('banco'), texto.includes('sem backup') || texto.includes('no backup')];
            await s.recoverRun();
            const rodou = sent.includes('ServerRecoverRun');
            await s.recoverCancel();
            return JSON.stringify({ aposErro, start, resumo, rodou, fase: s.recoverInfo.phase,
                chaveNoEstado: JSON.stringify(s).includes('AGE-SECRET-KEY') });
        })()"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["aposErro"],
        serde_json::json!(["", "", "https://github.com/org/estado.git"]),
        "{out}"
    );
    assert_eq!(
        v["start"]["remote"], "https://github.com/org/estado.git",
        "{out}"
    );
    assert_eq!(v["start"]["recovery_key"], "AGE-SECRET-KEY-1ABC", "{out}");
    assert_eq!(v["start"]["token"], "tok");
    assert_eq!(v["start"]["branch"], serde_json::Value::Null);
    assert_eq!(
        v["resumo"],
        serde_json::json!(["ready", true, true]),
        "{out}"
    );
    assert_eq!(v["rodou"], true, "{out}");
    assert_eq!(v["fase"], "idle", "{out}");
    assert_eq!(
        v["chaveNoEstado"], false,
        "a chave não pode ficar no estado do app: {out}"
    );
}

/// Portas TCP do host na aba Domains (`docs/arquivo/plano-multiplas-portas-tcp.md`): a lista nova, o legado
/// (`host_port` sozinho) virando a primeira linha, adicionar com a porta do container e o rótulo,
/// apagar, e as sugestões preenchendo o formulário. A webui nunca tinha tido editor de porta TCP.
#[tokio::test]
async fn tcp_ports_editor_lists_adds_deletes_and_suggests() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            s.view = 'service';
            s.selectedServiceId = 'svc_s3';
            s.serviceTab = 'domains';
            s.serviceLoading = false;
            s.serviceDeployments = [];
            s.serviceDetail = {
                id: 'svc_s3', status: 'Running', live_container_id: null,
                spec: { name: 's3', port: 9000, replicas: 1, project_id: 'prj_1', product: 'rustfs',
                    host_port: 20002, env_vars: [], domains: [],
                    source: { Compose: { content: 'services:\n  rp_s3:\n    image: rustfs/rustfs\n' } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
            };
            window.__saved = [];
            s.saveServiceSpec = async (spec, msg) => { window.__saved.push({ spec, msg }); return { ok: true }; };
            window.__toasts = [];
            s.toastError = (m) => window.__toasts.push(m);
            s.api.rpcChecked = async (cmd) => cmd.ServicePortSuggestions
                ? { ok: true, value: { ServicePortSuggestions: [
                    { port: 9001, label: 'console', sources: ['listening', 'product'], internal: false },
                ] } }
                : { ok: false, error: 'inesperado' };
        })()"#,
    )
    .await
    .expect("abre a aba Domains do s3");

    let d = "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]'))";

    // O legado (`host_port` sozinho) aparece como uma porta para a porta do serviço.
    assert_eq!(
        eval_str(&page, &format!("(() => JSON.stringify({d}.tcpPorts))()")).await,
        r#"[{"hostPort":20002,"containerPort":9000,"label":""}]"#
    );
    assert!(
        visible(
            &page,
            "[x-data=\"serviceDetail\"] input[x-model=\"newTcpContainer\"]"
        )
        .await,
        "a aba Domains deveria mostrar o formulário de porta TCP"
    );
    // Nada espremido: cada controle do bloco de portas tem altura de verdade (no app desktop, o bloco
    // novo ficou sem rolagem e cortado; aqui o `.env` cresce com o conteúdo e a página rola).
    let heights = eval_str(
        &page,
        r#"(() => JSON.stringify(['input[x-model="newTcpHost"]', 'input[x-model="newTcpContainer"]',
            'input[x-model="newTcpLabel"]', 'button[\\@click="addTcpPort()"]', 'button[\\@click="suggestPorts()"]']
            .map((sel) => Math.round(document.querySelector('[x-data="serviceDetail"] ' + sel).getBoundingClientRect().height))))()"#,
    )
    .await;
    let heights: Vec<u32> = serde_json::from_str(&heights).unwrap();
    assert!(
        heights.iter().all(|h| *h >= 20),
        "controle espremido: alturas {heights:?}"
    );

    // Sugestões: o botão vem com nome e "escutando agora"; clicar preenche porta e rótulo.
    let picked = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; await d.suggestPorts(); d.pickSuggestion(d.portSuggestions[0]); return JSON.stringify({{ text: d.portSuggestions[0].text, port: d.newTcpContainer, label: d.newTcpLabel }}); }})()"),
    )
    .await;
    let p: serde_json::Value = serde_json::from_str(&picked).unwrap();
    assert_eq!(p["text"], "9001 console (listening now)");
    assert_eq!(p["port"], "9001");
    assert_eq!(p["label"], "console");

    // Adicionar (host vazio = automática): o legado vira lista, `host_port` vai nulo, e a nova entra.
    let added = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; await d.addTcpPort(); const x = window.__saved[0]; return JSON.stringify({{ msg: x.msg, host_port: x.spec.host_port, tcp: x.spec.tcp_ports, cleared: d.newTcpContainer === '' && d.newTcpLabel === '' }}); }})()"),
    )
    .await;
    let a: serde_json::Value = serde_json::from_str(&added).unwrap();
    assert_eq!(a["msg"], "port added");
    assert!(
        a["host_port"].is_null(),
        "o espelho é do daemon: o cliente manda null"
    );
    assert_eq!(
        a["tcp"],
        serde_json::json!([
            { "host_port": 20002 },
            { "host_port": 0, "container_port": 9001, "label": "console" }
        ])
    );
    assert_eq!(a["cleared"], true, "o formulário zera depois de salvar");

    // Porta inválida: só avisa, não salva.
    let invalid = eval_str(
        &page,
        &format!("(async () => {{ const d = {d}; d.newTcpHost = '70000'; await d.addTcpPort(); d.newTcpHost = ''; return JSON.stringify({{ saved: window.__saved.length, toasts: window.__toasts.length }}); }})()"),
    )
    .await;
    assert_eq!(invalid, r#"{"saved":1,"toasts":1}"#);

    // Apagar: a lista volta sem a porta, também com `host_port` nulo.
    let deleted = eval_str(
        &page,
        &format!("(async () => {{ const s = Alpine.store('app'); s.serviceDetail.spec.tcp_ports = [{{ host_port: 20002 }}, {{ host_port: 20003, container_port: 9001, label: 'console' }}]; s.serviceDetail.spec.host_port = 20002; const before = window.__saved.length; window.confirm = () => false; await {d}.deleteTcpPort(20002); const refused = window.__saved.length === before; window.confirm = () => true; await {d}.deleteTcpPort(20002); const x = window.__saved[window.__saved.length - 1]; return JSON.stringify({{ refused, msg: x.msg, host_port: x.spec.host_port, tcp: x.spec.tcp_ports }}); }})()"),
    )
    .await;
    let del: serde_json::Value = serde_json::from_str(&deleted).unwrap();
    assert_eq!(
        del["refused"], true,
        "recusar a confirmação não remove a porta"
    );
    assert_eq!(del["msg"], "port removed");
    assert!(del["host_port"].is_null());
    assert_eq!(
        del["tcp"],
        serde_json::json!([{ "host_port": 20003, "container_port": 9001, "label": "console" }])
    );

    // Tela de Ingress: uma linha por porta, com a porta do container e o rótulo.
    let rows = eval_str(
        &page,
        r#"(async () => {
            const { hostPortRows } = await import('./format.js');
            const spec = { name: 's3', project_id: 'prj_1', port: 9000,
                tcp_ports: [{ host_port: 20003, container_port: 9001, label: 'console' }, { host_port: 20002 }] };
            const legado = { name: 'db', project_id: 'prj_1', port: 5432, host_port: 20000 };
            return JSON.stringify(hostPortRows([{ svc: { id: 'a', spec }, proj: 'p' }, { svc: { id: 'b', spec: legado }, proj: 'p' }])
                .map((r) => [r.hostPort, r.containerPort]));
        })()"#,
    )
    .await;
    assert_eq!(
        rows,
        r#"[[20000,5432],[20002,9000],[20003,"9001 (console)"]]"#
    );

    assert_eq!(
        errors(&page).await,
        Vec::<String>::new(),
        "sem erros de JS no console"
    );
}

/// Vínculos (aba Advanced, docs/plano-recursos-e-vinculos.md): as opções são o mesmo projeto e só o
/// banco compartilhado destravado de outro projeto; vincular grava `links` com o prefixo em
/// maiúsculas; desfazer pede confirmação (recusar não grava); "quem usa" lista os consumidores.
#[tokio::test]
async fn links_section_lists_options_saves_and_confirms() {
    let addr = spawn_static_server().await;
    let (browser, _handler) = launch().await;
    let page = open_page(&browser, addr).await;
    seed_connected(&page).await;

    page.evaluate(
        r#"(() => {
            const s = Alpine.store('app');
            const svc = (id, name, project_id, extra) => ({ service: { id, status: 'Running',
                spec: Object.assign({ name, project_id, port: 5432, links: [] }, extra || {}) },
                project_name: project_id === 'prj_1' ? 'loja' : 'infra' });
            s.snap.services = [
                svc('svc_api', 'api', 'prj_1'),
                svc('svc_db', 'db', 'prj_1', { product: 'postgres' }),
                svc('svc_worker', 'worker', 'prj_1', { links: [{ service_id: 'svc_db' }] }),
                svc('svc_outro', 'outro', 'prj_2'),
                svc('svc_pg', 'pg', 'prj_2', { shared: { accept_links: true } }),
                svc('svc_pg_trancado', 'pg2', 'prj_2', { shared: { accept_links: false } }),
            ];
            s.view = 'service';
            s.selectedServiceId = 'svc_api';
            s.serviceTab = 'advanced';
            s.serviceLoading = false;
            s.serviceDeployments = [];
            s.serviceDetail = {
                id: 'svc_api', status: 'Running', live_container_id: null,
                spec: { name: 'api', port: 3000, replicas: 1, project_id: 'prj_1', env_vars: [], domains: [],
                    links: [], depends_on: [],
                    source: { Registry: { image: 'nginx' } },
                    healthcheck: { kind: 'None', interval_secs: 5, timeout_secs: 3, retries: 10, start_period_secs: 5 } },
            };
            window.__saved = [];
            s.saveServiceSpec = async (spec, msg) => { window.__saved.push({ spec, msg }); return { ok: true }; };
        })()"#,
    )
    .await
    .expect("abre a aba Advanced do api");

    let d = "Alpine.$data(document.querySelector('[x-data=\"serviceDetail\"]'))";
    let options: Vec<String> = serde_json::from_str(
        &eval_str(
            &page,
            &format!("(() => JSON.stringify({d}.linkOptions.map((o) => o.id)))()"),
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        options,
        vec!["svc_db", "svc_worker", "svc_pg"],
        "mesmo projeto + compartilhado aberto"
    );
    assert!(
        visible(
            &page,
            "[x-data=\"serviceDetail\"] button[\\@click=\"linkAdd()\"]"
        )
        .await,
        "a aba Advanced deveria mostrar a seção de vínculos"
    );

    // O campo de recurso segue o serviço escolhido: database num Postgres,
    // nenhum num serviço sem recurso.
    let fields = eval_str(
        &page,
        &format!("(async () => {{ const shown = () => [...document.querySelectorAll('[x-data=\"serviceDetail\"] .link-resource')].filter((e) => e.offsetParent !== null).map((e) => e.querySelector('text').textContent.trim()); {d}.advancedLinkAddId = 'svc_db'; await Alpine.nextTick(); const db = shown(); {d}.advancedLinkAddId = 'svc_worker'; await Alpine.nextTick(); const worker = shown(); return JSON.stringify({{ db, worker, kind: {d}.linkTargetKind }}); }})()"),
    )
    .await;
    assert_eq!(
        fields,
        r#"{"db":["DATABASE (OPTIONAL)"],"worker":[],"kind":null}"#
    );
    let ignored = eval_str(
        &page,
        &format!("(async () => {{ {d}.advancedLinkAddId = 'svc_worker'; {d}.advancedLinkResource = 'x'; await {d}.linkAdd(); return JSON.stringify(window.__saved.at(-1).spec.links); }})()"),
    )
    .await;
    assert_eq!(
        ignored, r#"[{"service_id":"svc_worker"}]"#,
        "sem recurso no serviço escolhido, o texto digitado é ignorado"
    );
    let saved = eval_str(
        &page,
        &format!("(async () => {{ {d}.advancedLinkAddId = 'svc_db'; {d}.advancedLinkPrefix = 'main'; {d}.advancedLinkResource = ' Pedidos '; {d}.advancedLinkPublicRead = true; await {d}.linkAdd(); const x = window.__saved.at(-1); return JSON.stringify(x.spec.links); }})()"),
    )
    .await;
    assert_eq!(
        saved,
        r#"[{"service_id":"svc_db","env_prefix":"MAIN","resource":{"name":"pedidos","public_read":false}}]"#
    );

    // Nomes das variáveis: trocar a URL, não entregar a PORT; o resto fica fora do mapa (padrão).
    let renamed = eval_str(
        &page,
        &format!("(async () => {{ {d}.linkFormReset(); {d}.advancedLinkAddId = 'svc_db'; {d}.linkTargetChanged(); await Alpine.nextTick(); const rows = {d}.linkNameRows.map((r) => r.suffix + '=' + r.placeholder); {d}.advancedLinkNames.URL = 'database_url'; {d}.advancedLinkDeliver.PORT = false; await {d}.linkAdd(); return JSON.stringify({{ rows, links: window.__saved.at(-1).spec.links }}); }})()"),
    )
    .await;
    assert_eq!(
        renamed,
        r#"{"rows":["HOST=DB_HOST","PORT=DB_PORT","URL=DB_URL","DATABASE=DB_DATABASE"],"links":[{"service_id":"svc_db","env_names":{"PORT":"","URL":"DATABASE_URL"}}]}"#
    );
    // Na lista: os nomes efetivos, e o aviso quando um deles está digitado no serviço (ele ganha).
    // Editar carrega o vínculo no formulário e salvar o substitui no mesmo lugar.
    let listed = eval_str(
        &page,
        &format!("(async () => {{ const sd = Alpine.store('app').serviceDetail; sd.spec.links = [{{ service_id: 'svc_db', env_names: {{ URL: 'DATABASE_URL', PORT: '' }} }}, {{ service_id: 'svc_worker' }}]; sd.spec.env_vars = [{{ key: 'DATABASE_URL', value: {{ Plain: 'x' }} }}]; await Alpine.nextTick(); const rows = {d}.linkRows.map((r) => [r.vars, r.warning]); const warnShown = [...document.querySelectorAll('[x-data=\"serviceDetail\"] .link-row__warning')].some((e) => e.offsetParent !== null && e.textContent.includes('DATABASE_URL')); {d}.linkEdit('svc_db'); await Alpine.nextTick(); const loaded = [{d}.advancedLinkEditing, {d}.advancedLinkNames.URL, {d}.advancedLinkDeliver.PORT, {d}.advancedLinkDeliver.HOST]; {d}.advancedLinkNames.URL = ''; {d}.advancedLinkDeliver.PORT = true; await {d}.linkAdd(); return JSON.stringify({{ rows, warnShown, loaded, links: window.__saved.at(-1).spec.links, msg: window.__saved.at(-1).msg, after: {d}.advancedLinkEditing }}); }})()"),
    )
    .await;
    assert_eq!(
        listed,
        r#"{"rows":[["DB_HOST · DATABASE_URL","Already typed, the typed one wins: DATABASE_URL (service)"],["WORKER_HOST · WORKER_PORT · WORKER_URL",""]],"warnShown":true,"loaded":["svc_db","DATABASE_URL",false,true],"links":[{"service_id":"svc_db"},{"service_id":"svc_worker"}],"msg":"link saved","after":""}"#
    );

    let deleted = eval_str(
        &page,
        &format!("(async () => {{ Alpine.store('app').serviceDetail.spec.links = [{{ service_id: 'svc_db' }}]; const before = window.__saved.length; window.confirm = () => false; await {d}.linkDelete('svc_db'); const refused = window.__saved.length === before; window.confirm = () => true; await {d}.linkDelete('svc_db'); return JSON.stringify({{ refused, links: window.__saved.at(-1).spec.links }}); }})()"),
    )
    .await;
    assert_eq!(deleted, r#"{"refused":true,"links":[]}"#);

    // "Quem usa": do lado do db, o worker aparece.
    let consumers = eval_str(
        &page,
        r#"(() => { const s = Alpine.store('app'); s.selectedServiceId = 'svc_db'; s.serviceDetail = Object.assign({}, s.serviceDetail, { id: 'svc_db', spec: Object.assign({}, s.serviceDetail.spec, { name: 'db', links: [] }) }); return JSON.stringify(Alpine.$data(document.querySelector('[x-data="serviceDetail"]')).consumerRows.map((c) => c.id)); })()"#,
    )
    .await;
    assert_eq!(consumers, r#"["svc_worker"]"#);
}
