//! Acesso de agente servido pelo próprio daemon: a chave `rpk1_…`, o
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

/// `POST /api/login`: troca a chave `rpk1_…` pelo token de acesso.
///
/// Aceita `{"key":"rpk1_…"}`, uma string JSON ou a chave crua no corpo. O token
/// devolvido é o próprio `[api].token` (decisão A do plano): o daemon não tem
/// sessões, e emitir uma por cima não daria garantia nova enquanto o token-mestre
/// valer para tudo.
///
/// `host` é o header `Host` da requisição: as URLs devolvidas usam o endereço
/// pelo qual o agente **de fato** chegou (atrás de proxy, ou em loopback, ele
/// difere do gravado na chave); sem ele, vale o da chave.
pub fn login(body: &[u8], expected: Option<&str>, host: Option<&str>) -> (StatusCode, Value) {
    let raw = match extract_key(body) {
        Ok(raw) => raw,
        Err(fail) => return (StatusCode::BAD_REQUEST, format_error(&fail)),
    };
    let key = match AccessKey::decode(&raw) {
        Ok(key) => key,
        Err(fail) => return (StatusCode::BAD_REQUEST, format_error(&fail)),
    };

    if let Some(expected) = expected {
        if !super::http_api::constant_time_eq(key.token.as_bytes(), expected.as_bytes()) {
            return (
                StatusCode::UNAUTHORIZED,
                json!({
                    "error": {
                        "code": "invalid_key",
                        "message": "a chave está bem formada, mas o token dentro dela não é o \
                                    deste servidor — ela é de outra instalação, ou o token foi \
                                    trocado no config.toml",
                    }
                }),
            );
        }
    }

    let server = server_url(&key.url, host);
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

/// O esquema vem da chave (o listener não sabe se há TLS na frente dele); o
/// host, do header quando houver.
fn server_url(key_url: &str, host: Option<&str>) -> String {
    let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) else {
        return key_url.to_owned();
    };
    let esquema = key_url.split_once("://").map(|(e, _)| e).unwrap_or("http");
    format!("{esquema}://{host}")
}

/// Tira a chave do corpo do login. `Err` só quando o corpo é JSON mas não traz
/// a chave em nenhum formato reconhecível.
fn extract_key(body: &[u8]) -> Result<String, KeyFormatError> {
    let texto = String::from_utf8_lossy(body);
    match serde_json::from_str::<Value>(&texto) {
        Ok(Value::String(s)) => Ok(s),
        Ok(Value::Object(m)) => m
            .get("key")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(KeyFormatError::Empty),
        // Não é JSON: o corpo É a chave (`curl -d rpk1_…`).
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
                "body": { "key": "rpk1_<base64url de {\"u\":\"https://host:porta\",\"t\":\"<token>\"}>" },
                "also_accepted": "a chave crua como corpo (Content-Type qualquer)",
                "example": "curl -s https://host:porta/api/login -d '{\"key\":\"rpk1_…\"}'",
                "where_to_get_one": "na VPS: `rustployd access-key`; numa sessão já aberta: \
                                     Configurações → Copiar chave de acesso (GUI ou webui)",
            }
        }
    })
}

/// Monta a chave deste servidor a partir da config: o endereço público do
/// daemon (o mesmo do webhook) e o `[api].token`.
pub fn access_key_for(base_url: &str, token: Option<&str>) -> Option<String> {
    let token = token.map(str::trim).filter(|t| !t.is_empty())?;
    Some(
        AccessKey {
            url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
        .encode(),
    )
}

/// Imprime a chave de acesso e sai — `rustployd access-key`.
pub fn print_access_key_and_exit(config: &shared::RustployConfig) -> ! {
    let base = config.api.public_base_url(&super::outbound_ip());
    match access_key_for(&base, config.api.token.as_deref()) {
        Some(key) => {
            println!("{key}");
            std::process::exit(0);
        }
        None => {
            eprintln!(
                "sem [api].token configurado — a API está aberta sem autenticação e não \
                 há chave a gerar. Defina api.token (ou RUSTPLOY_API_TOKEN)."
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chave() -> String {
        access_key_for("http://10.0.0.5:9797/", Some("segredo")).unwrap()
    }

    #[test]
    fn login_com_chave_certa_devolve_o_token_e_o_caminho_do_agente() {
        let corpo = json!({ "key": chave() }).to_string();
        let (status, resp) = login(corpo.as_bytes(), Some("segredo"), None);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["access_token"], "segredo");
        assert_eq!(resp["server"], "http://10.0.0.5:9797");
        assert_eq!(resp["ai_agent"]["mcp"], "http://10.0.0.5:9797/mcp");
    }

    #[test]
    fn login_aceita_a_chave_crua_e_string_json() {
        assert_eq!(
            login(chave().as_bytes(), Some("segredo"), None).0,
            StatusCode::OK
        );
        let como_string = serde_json::to_string(&chave()).unwrap();
        assert_eq!(
            login(como_string.as_bytes(), Some("segredo"), None).0,
            StatusCode::OK
        );
    }

    #[test]
    fn formato_errado_explica_o_formato_certo() {
        let (status, resp) = login(br#"{"key":"segredo"}"#, Some("segredo"), None);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp["error"]["code"], "missing_prefix");
        assert_eq!(resp["error"]["expected"]["path"], "/api/login");

        let (status, resp) = login(br#"{"token":"x"}"#, Some("segredo"), None);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp["error"]["code"], "empty_key");
    }

    #[test]
    fn token_de_outra_instalacao_e_401_sem_ecoar_o_token() {
        let (status, resp) = login(chave().as_bytes(), Some("outro"), None);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!resp.to_string().contains("segredo"));
    }

    #[test]
    fn urls_do_agente_usam_o_host_pelo_qual_ele_chegou() {
        let (_, resp) = login(chave().as_bytes(), Some("segredo"), Some("127.0.0.1:9797"));
        assert_eq!(resp["server"], "http://127.0.0.1:9797");
        assert_eq!(resp["ai_agent"]["mcp"], "http://127.0.0.1:9797/mcp");
    }

    #[test]
    fn sem_token_configurado_nao_ha_chave() {
        assert_eq!(access_key_for("http://x:1", None), None);
        assert_eq!(access_key_for("http://x:1", Some("  ")), None);
    }
}
