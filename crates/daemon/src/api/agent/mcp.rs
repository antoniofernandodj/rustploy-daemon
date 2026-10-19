//! Servidor MCP em `POST /mcp` — transporte Streamable HTTP, sempre respondendo
//! `application/json` (o spec permite; não abrimos stream SSE).
//!
//! Feito à mão em cima do hyper, sem crate de MCP: o protocolo que um servidor
//! só de ferramentas precisa é meia dúzia de métodos JSON-RPC, e a regra do
//! projeto é não trazer um segundo framework HTTP. Bônus do JSON simples: um
//! agente que só tem `curl` consegue chamar as mesmas ferramentas.

use hyper::StatusCode;
use serde_json::{Value, json};

use super::{MANUAL, tools};
use crate::bootstrap::state::AppState;

/// Versões do protocolo que este servidor fala, da mais nova para a mais velha.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const MANUAL_URI: &str = "rustploy://manual";

/// Atende um corpo de `POST /mcp`. `None` = era notificação/resposta, que o
/// transporte responde com `202` sem corpo.
///
/// `server` é o `host:porta` deste daemon (o header `Host` pelo qual o agente
/// chegou, ou o endereço público): vai no `serverInfo` e no topo das
/// `instructions`, para um agente com vários rustploy registrados saber em
/// qual está mexendo — todos se chamam `rustploy` e servem o mesmo manual.
pub async fn handle(state: &AppState, body: &[u8], server: &str) -> Option<(StatusCode, Value)> {
    let msg: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Some((
                StatusCode::BAD_REQUEST,
                error(Value::Null, -32700, format!("JSON inválido: {e}")),
            ));
        }
    };

    if msg.is_array() {
        return Some((
            StatusCode::BAD_REQUEST,
            error(
                Value::Null,
                -32600,
                "lotes JSON-RPC não são aceitos — uma mensagem por POST",
            ),
        ));
    }

    // Sem `id` = notificação (`notifications/initialized`, `…/cancelled`); sem
    // `method` = resposta a algo que nunca pedimos. Nos dois casos, só 202.
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str)?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    let resultado = match method {
        "initialize" => Ok(initialize(&params, server)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "tools/call" => tools_call(state, &params).await,
        "resources/list" => Ok(json!({
            "resources": [{
                "uri": MANUAL_URI,
                "name": "manual",
                "title": "Manual do agente rustploy",
                "mimeType": "text/markdown",
            }]
        })),
        "resources/read" => resources_read(&params),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        outro => Err((-32601, format!("método {outro:?} não existe"))),
    };

    Some(match resultado {
        Ok(result) => (
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        ),
        Err((code, message)) => (StatusCode::OK, error(id, code, message)),
    })
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn initialize(params: &Value, server: &str) -> Value {
    // Ecoa a versão do cliente quando a conhecemos; senão, a nossa mais nova
    // (o cliente decide se aceita).
    let pedida = params.get("protocolVersion").and_then(Value::as_str);
    let versao = pedida
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);

    json!({
        "protocolVersion": versao,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "listChanged": false, "subscribe": false },
        },
        "serverInfo": {
            "name": "rustploy",
            "title": format!("Rustploy daemon em {server}"),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": format!(
            "Este servidor MCP é o daemon rustploy em **{server}**. Se houver mais de \
             um rustploy registrado, confirme que é este o que o usuário pediu antes \
             de qualquer ação que altere algo.\n\n{MANUAL}"
        ),
    })
}

fn tools_list() -> Value {
    let lista: Vec<Value> = tools::TOOLS
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.input_schema)(),
            })
        })
        .collect();
    json!({ "tools": lista })
}

/// Falha de ferramenta vai no **resultado** (`isError`), não como erro
/// JSON-RPC: é assim que o modelo do outro lado chega a ver a causa.
async fn tools_call(state: &AppState, params: &Value) -> Result<Value, (i64, String)> {
    let nome = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call sem \"name\"".to_owned()))?;
    if !tools::TOOLS.iter().any(|t| t.name == nome) {
        return Err((
            -32602,
            format!("ferramenta {nome:?} não existe — veja tools/list"),
        ));
    }
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    Ok(match tools::call(state, nome, &args).await {
        Ok(valor) => {
            // `manual` devolve markdown: texto cru lê melhor que JSON escapado.
            let texto = match valor.get("manual").and_then(Value::as_str) {
                Some(md) if nome == "manual" => md.to_owned(),
                _ => serde_json::to_string_pretty(&valor).unwrap_or_default(),
            };
            let mut out = json!({
                "content": [{ "type": "text", "text": texto }],
                "isError": false,
            });
            // `structuredContent` precisa ser objeto.
            if valor.is_object() {
                out["structuredContent"] = valor;
            }
            out
        }
        Err(falha) => json!({
            "content": [{ "type": "text", "text": format!("{}: {}", falha.code, falha.message) }],
            "isError": true,
        }),
    })
}

fn resources_read(params: &Value) -> Result<Value, (i64, String)> {
    match params.get("uri").and_then(Value::as_str) {
        Some(MANUAL_URI) => Ok(json!({
            "contents": [{ "uri": MANUAL_URI, "mimeType": "text/markdown", "text": MANUAL }]
        })),
        Some(outro) => Err((-32002, format!("recurso {outro:?} não existe"))),
        None => Err((-32602, "resources/read sem \"uri\"".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_ecoa_versao_conhecida_e_manda_o_manual() {
        let r = initialize(
            &json!({ "protocolVersion": "2025-03-26" }),
            "rp.exemplo.com:9797",
        );
        assert_eq!(r["protocolVersion"], "2025-03-26");
        let instructions = r["instructions"].as_str().unwrap();
        assert!(instructions.ends_with(MANUAL));
        assert!(
            instructions
                .starts_with("Este servidor MCP é o daemon rustploy em **rp.exemplo.com:9797**")
        );
        assert_eq!(r["serverInfo"]["name"], "rustploy");
        assert_eq!(
            r["serverInfo"]["title"],
            "Rustploy daemon em rp.exemplo.com:9797"
        );

        let r = initialize(&json!({ "protocolVersion": "1999-01-01" }), "x:1");
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn tools_list_traz_todas_com_schema() {
        let r = tools_list();
        let nomes: Vec<&str> = r["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(nomes.contains(&"deploy") && nomes.contains(&"rpc"));
        assert!(r["tools"][0]["inputSchema"].is_object());
    }

    #[test]
    fn manual_como_recurso() {
        let r = resources_read(&json!({ "uri": MANUAL_URI })).unwrap();
        assert_eq!(r["contents"][0]["text"], MANUAL);
        assert!(resources_read(&json!({ "uri": "x://y" })).is_err());
    }
}
