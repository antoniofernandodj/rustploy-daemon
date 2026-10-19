//! Acesso de agente servido pelo próprio daemon: a chave de acesso, o
//! `POST /api/login`, o manual e o servidor MCP em `POST /mcp`.
//!
//! Substituiu a ponte HTTP local que morava no app GUI
//! (`rustploy-gui/src/agent/`), que só funcionava com a janela aberta na mesma
//! máquina. Ver docs/plano-chave-de-acesso-e-mcp.md.

pub mod access_key;
pub mod mcp;
pub mod tools;

use hyper::StatusCode;
use serde_json::{Value, json};

use access_key::{AccessKey, KeyFormatError};

/// O manual do agente — fonte única do texto servido em `GET /api/agent/manual`,
/// nas `instructions` do MCP e no recurso `rustploy://manual`.
pub const MANUAL: &str = include_str!("manual.md");

/// `POST /api/login`: troca o token de conexão pelo token da API.
///
/// Aceita `{"key":"<token de conexão>"}`, uma string JSON ou o token de
/// conexão cru no corpo. O token devolvido é o próprio `[api].token` (decisão A
/// do plano): o daemon não tem sessões. A GUI e a webui não passam por aqui —
/// elas decodificam o token de conexão e usam o token da API direto; este
/// endpoint existe para um agente que só recebeu o token de conexão.
///
/// `host` é o header `Host` da requisição: as URLs devolvidas usam o endereço
/// pelo qual o agente **de fato** chegou (atrás de proxy, ou em loopback, ele
/// difere do gravado no token); sem ele, vale o do token. `scheme` é o deste
/// listener (`https` quando ele termina TLS), que o token não carrega.
pub fn login(
    body: &[u8],
    expected: Option<&str>,
    host: Option<&str>,
    scheme: &str,
) -> (StatusCode, Value) {
    let raw = match extract_key(body) {
        Ok(raw) => raw,
        Err(fail) => return (StatusCode::BAD_REQUEST, format_error(&fail)),
    };
    let key = match AccessKey::decode(&raw) {
        Ok(key) => key,
        Err(fail) => return (StatusCode::BAD_REQUEST, format_error(&fail)),
    };

    if let Some(expected) = expected
        && !crate::api::http::constant_time_eq(key.token.as_bytes(), expected.as_bytes())
    {
        return (
            StatusCode::UNAUTHORIZED,
            json!({
                "error": {
                    "code": "invalid_key",
                    "message": "o token de conexão está bem formado, mas o token da API \
                                dentro dele não é o deste servidor — ele é de outra \
                                instalação, ou o token foi trocado no config.toml",
                }
            }),
        );
    }

    let authority = host
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or(&key.address);
    let server = format!("{scheme}://{authority}");
    (
        StatusCode::OK,
        json!({
            "access_token": key.token,
            "token_type": "Bearer",
            "server": server,
            "next": "Guarde access_token e mande 'Authorization: Bearer <access_token>' em toda chamada.",
            "ai_agent": {
                "if_you_are": "um agente de IA",
                "mcp": format!("{server}/mcp"),
                "manual": format!("{server}/api/agent/manual"),
                "instructions": "Leia o manual com GET e o mesmo Bearer, ou conecte no MCP \
                                 (Streamable HTTP). Tudo o que um agente pode fazer neste \
                                 servidor está lá.",
            },
        }),
    )
}

/// Tira o token de conexão do corpo do login. `Err` só quando o corpo é JSON
/// mas não o traz em nenhum formato reconhecível.
fn extract_key(body: &[u8]) -> Result<String, KeyFormatError> {
    let texto = String::from_utf8_lossy(body);
    match serde_json::from_str::<Value>(&texto) {
        Ok(Value::String(s)) => Ok(s),
        Ok(Value::Object(m)) => m
            .get("key")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(KeyFormatError::Empty),
        // Não é JSON: o corpo É o token de conexão (`curl -d <token>`).
        _ => Ok(texto.into_owned()),
    }
}

/// O `400` do login: o que falhou **e** o formato certo, para um agente
/// acertar na tentativa seguinte sem ler documentação nenhuma.
fn format_error(fail: &KeyFormatError) -> Value {
    json!({
        "error": {
            "code": fail.code(),
            "message": fail.message(),
            "expected": {
                "method": "POST",
                "path": "/api/login",
                "body": { "key": "<base64 de host:porta::token>" },
                "key_format": "base64 (com ou sem padding, alfabeto padrão ou url-safe) de \
                               `host:porta::token`, sem esquema — ex. \
                               base64(\"rustploy.exemplo.com:9797::a3f9\")",
                "also_accepted": "o token de conexão cru como corpo (Content-Type qualquer)",
                "example": "curl -s https://host:porta/api/login -d '{\"key\":\"<token de conexão>\"}'",
                "where_to_get_one": "no config.toml do daemon, [api].connection_token; na VPS: \
                                     `rustployd access-key`; numa sessão já aberta: \
                                     Configurações → Copiar token de conexão (GUI ou webui)",
            }
        }
    })
}

/// `host:porta` público deste daemon: `[api].domain` quando definido, senão o
/// IP de saída; a porta é sempre a do listener (mesmo a 443).
pub fn public_address(api: &crate::platform::config::ApiConfig, fallback_host: &str) -> String {
    let host = api
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(fallback_host);
    format!("{host}:{}", api.port)
}

/// Monta o token de conexão a partir do endereço e do `[api].token`.
pub fn access_key_for(address: &str, token: Option<&str>) -> Option<String> {
    let token = token.map(str::trim).filter(|t| !t.is_empty())?;
    Some(
        AccessKey {
            address: address.trim().to_owned(),
            token: token.to_owned(),
        }
        .encode(),
    )
}

/// Imprime o token de conexão e sai — `rustployd access-key`.
pub fn print_access_key_and_exit(config: &crate::platform::config::RustployConfig) -> ! {
    let address = public_address(&config.api, &crate::bootstrap::state::outbound_ip());
    match access_key_for(&address, config.api.token.as_deref()) {
        Some(key) => {
            println!("{key}");
            std::process::exit(0);
        }
        None => {
            eprintln!(
                "sem [api].token configurado — a API está aberta sem autenticação e não \
                 há token de conexão a gerar. Defina api.token (ou RUSTPLOY_API_TOKEN)."
            );
            std::process::exit(1);
        }
    }
}

/// Chave do `config.toml` onde o daemon deixa o token de conexão pronto.
pub const CONNECTION_TOKEN_KEY: &str = "connection_token";

/// Grava `connection_token = "<token de conexão>"` logo abaixo do `token` da
/// seção `[api]` do `config.toml` em uso, para quem administra copiar dali.
///
/// O daemon não lê esse campo (o serde ignora chave desconhecida, então o
/// parse continua valendo): é só o token de conexão pré-calculado, refeito a
/// cada boot — trocar `token` ou `domain` e reiniciar atualiza a linha. O
/// arquivo só é reescrito quando a linha muda, e nada acontece quando o token
/// veio de `RUSTPLOY_API_TOKEN` em vez do arquivo.
pub fn write_connection_token(
    config: &crate::platform::config::RustployConfig,
) -> anyhow::Result<()> {
    let Some(path) = config_file_in_use() else {
        return Ok(());
    };
    let Some(token) = config.api.token.as_deref() else {
        return Ok(());
    };
    let address = public_address(&config.api, &crate::bootstrap::state::outbound_ip());
    let Some(key) = access_key_for(&address, Some(token)) else {
        return Ok(());
    };
    let contents = std::fs::read_to_string(&path)?;
    if let Some(updated) = with_connection_token(&contents, token, &key) {
        std::fs::write(&path, updated)?;
        tracing::info!(path = %path.display(), "token de conexão gravado no config.toml");
    }
    Ok(())
}

/// O mesmo arquivo que `RustployConfig::load` escolhe: o primeiro da lista que
/// existe **e** faz parse.
fn config_file_in_use() -> Option<std::path::PathBuf> {
    let candidates = [
        std::env::var("RUSTPLOY_CONFIG")
            .ok()
            .map(std::path::PathBuf::from),
        Some(std::path::PathBuf::from("/etc/rustploy/config.toml")),
        crate::platform::config::user_home()
            .map(|home| std::path::PathBuf::from(home).join(".config/rustploy/config.toml")),
    ];
    candidates.into_iter().flatten().find(|path| {
        std::fs::read_to_string(path).ok().is_some_and(|text| {
            toml::from_str::<crate::platform::config::RustployConfig>(&text).is_ok()
        })
    })
}

/// O texto do `config.toml` com a linha do token de conexão no lugar (logo
/// abaixo do `token` de `[api]`), ou `None` quando nada muda — inclusive
/// quando o `token` da seção não é o que está valendo.
fn with_connection_token(contents: &str, api_token: &str, key: &str) -> Option<String> {
    let new_line = format!(
        "{CONNECTION_TOKEN_KEY} = \"{key}\"  # gerado pelo rustployd a cada boot: \
         base64(host:porta::token), para colar na GUI/webui"
    );
    let key_of = |line: &str| -> Option<String> {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            return None;
        }
        trimmed
            .split_once('=')
            .map(|(name, _)| name.trim().trim_matches('"').to_owned())
    };

    let mut output: Vec<String> = Vec::new();
    let mut in_api = false;
    let mut inserted = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_api = trimmed == "[api]";
        }
        if in_api && key_of(line).as_deref() == Some(CONNECTION_TOKEN_KEY) {
            continue;
        }
        output.push(line.to_owned());
        if in_api && !inserted && key_of(line).as_deref() == Some("token") {
            let value = toml::from_str::<toml::Table>(line)
                .ok()
                .and_then(|t| t.get("token")?.as_str().map(str::to_owned));
            if value.as_deref() != Some(api_token) {
                return None;
            }
            output.push(new_line.clone());
            inserted = true;
        }
    }
    if !inserted {
        return None;
    }
    let mut updated = output.join("\n");
    if contents.ends_with('\n') {
        updated.push('\n');
    }
    (updated != contents).then_some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chave() -> String {
        access_key_for("10.0.0.5:9797", Some("segredo")).unwrap()
    }

    #[test]
    fn login_com_chave_certa_devolve_o_token_e_o_caminho_do_agente() {
        let corpo = json!({ "key": chave() }).to_string();
        let (status, resp) = login(corpo.as_bytes(), Some("segredo"), None, "http");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["access_token"], "segredo");
        assert_eq!(resp["server"], "http://10.0.0.5:9797");
        assert_eq!(resp["ai_agent"]["mcp"], "http://10.0.0.5:9797/mcp");
    }

    #[test]
    fn login_aceita_a_chave_crua_e_string_json() {
        assert_eq!(
            login(chave().as_bytes(), Some("segredo"), None, "http").0,
            StatusCode::OK
        );
        let como_string = serde_json::to_string(&chave()).unwrap();
        assert_eq!(
            login(como_string.as_bytes(), Some("segredo"), None, "http").0,
            StatusCode::OK
        );
    }

    #[test]
    fn formato_errado_explica_o_formato_certo() {
        let (status, resp) = login(br#"{"key":"segredo"}"#, Some("segredo"), None, "http");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp["error"]["code"], "missing_separator");
        assert_eq!(resp["error"]["expected"]["path"], "/api/login");

        let (status, resp) = login(br#"{"token":"x"}"#, Some("segredo"), None, "http");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp["error"]["code"], "empty_key");
    }

    #[test]
    fn token_de_outra_instalacao_e_401_sem_ecoar_o_token() {
        let (status, resp) = login(chave().as_bytes(), Some("outro"), None, "http");
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!resp.to_string().contains("segredo"));
    }

    #[test]
    fn urls_do_agente_usam_o_host_pelo_qual_ele_chegou_e_o_esquema_do_listener() {
        let (_, resp) = login(
            chave().as_bytes(),
            Some("segredo"),
            Some("127.0.0.1:9797"),
            "https",
        );
        assert_eq!(resp["server"], "https://127.0.0.1:9797");
        assert_eq!(resp["ai_agent"]["mcp"], "https://127.0.0.1:9797/mcp");
    }

    #[test]
    fn sem_token_configurado_nao_ha_chave() {
        assert_eq!(access_key_for("x:1", None), None);
        assert_eq!(access_key_for("x:1", Some("  ")), None);
    }

    #[test]
    fn endereco_publico_sempre_leva_a_porta() {
        let mut api = crate::platform::config::ApiConfig {
            port: 443,
            domain: Some("rp.exemplo.com".into()),
            ..Default::default()
        };
        assert_eq!(public_address(&api, "10.0.0.5"), "rp.exemplo.com:443");
        api.domain = None;
        api.port = 9797;
        assert_eq!(public_address(&api, "10.0.0.5"), "10.0.0.5:9797");
    }

    const CONFIG: &str = "[daemon]\nlog_level = \"info\"\n\n[api]\nport = 9797\ntoken = \"abc\"\ndomain = \"rp.exemplo.com\"\n\n[registry]\ntoken = \"nao-e-este\"\n";

    #[test]
    fn grava_o_token_de_conexao_logo_abaixo_do_token_da_api() {
        let atualizado = with_connection_token(CONFIG, "abc", "CHAVE").unwrap();
        let linhas: Vec<&str> = atualizado.lines().collect();
        let posicao = linhas.iter().position(|l| *l == "token = \"abc\"").unwrap();
        assert!(linhas[posicao + 1].starts_with("connection_token = \"CHAVE\""));
        assert_eq!(atualizado.matches("connection_token").count(), 1);
        assert!(atualizado.ends_with('\n'));
        // O `[api]` continua válido (chave desconhecida é ignorada), com o mesmo token.
        let tabela: toml::Table = toml::from_str(&atualizado).unwrap();
        let api: crate::platform::config::ApiConfig = tabela["api"].clone().try_into().unwrap();
        assert_eq!(api.token.as_deref(), Some("abc"));

        // Segundo boot com a mesma chave: nada a reescrever.
        assert_eq!(with_connection_token(&atualizado, "abc", "CHAVE"), None);
        // Chave nova (token ou domínio trocados): a linha é substituída, não duplicada.
        let trocado = with_connection_token(&atualizado, "abc", "OUTRA").unwrap();
        assert_eq!(trocado.matches("connection_token").count(), 1);
        assert!(trocado.contains("connection_token = \"OUTRA\""));
    }

    #[test]
    fn nao_grava_quando_o_token_do_arquivo_nao_e_o_que_vale() {
        // Token vindo de RUSTPLOY_API_TOKEN: o do arquivo é outro.
        assert_eq!(with_connection_token(CONFIG, "do-ambiente", "CHAVE"), None);
        // Sem `token` em [api].
        assert_eq!(
            with_connection_token("[api]\nport = 9797\n", "abc", "CHAVE"),
            None
        );
    }
}
