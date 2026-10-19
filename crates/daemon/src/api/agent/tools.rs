//! As ferramentas do agente: os caminhos frequentes já resolvidos ("deployou?
//! por quê não?", "qual é o id deste serviço?") mais o `rpc` cru.
//!
//! Servidas pelo MCP (`mcp.rs`). Chamam [`dispatch`] direto, em processo — a
//! antiga ponte da GUI fazia o mesmo por HTTP. A lógica de desfecho e o índice
//! de serviços vieram de lá (`rustploy-gui/src/agent/routes.rs`, removido).

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shared::Command;

use super::super::routes::dispatch;
use crate::bootstrap::state::AppState;

/// De quanto em quanto tempo o `wait` de um deploy reconsulta o histórico.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Teto do `timeout_s` aceito em `deploy`.
const MAX_WAIT: Duration = Duration::from_secs(3600);

#[derive(Debug)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }
}

/// Uma ferramenta: nome, descrição para o agente e o JSON Schema dos argumentos.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: fn() -> Value,
}

fn no_arguments() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "manual",
        description: "O manual completo do agente rustploy (markdown). Leia primeiro se \
                      o seu cliente não mostrou as instruções do servidor.",
        input_schema: no_arguments,
    },
    ToolSpec {
        name: "status",
        description: "Versão/uptime do daemon e o estado da fila global de deploys.",
        input_schema: no_arguments,
    },
    ToolSpec {
        name: "list_services",
        description: "Índice achatado projeto→serviço: service_id, nome, projeto, status, \
                      origem da imagem e porta. Use para achar o service_id pelo nome.",
        input_schema: no_arguments,
    },
    ToolSpec {
        name: "list_deploys",
        description: "Últimos deploys de todos os serviços com o desfecho resolvido: \
                      ok=true (Live), ok=false (Failed, com `error` = a causa), ok=null \
                      (em andamento, ou Stopped/Pruning, que não são falha).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20 }
                }
            })
        },
    },
    ToolSpec {
        name: "deploy",
        description: "Dispara o deploy de um serviço e, com wait=true (padrão), só responde \
                      quando ele terminar — com ok, error e o fim do build log. Uma chamada \
                      responde 'funcionou?' e 'por quê não?'.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "service_id": { "type": "string", "description": "id do serviço (svc_…)" },
                    "service": { "type": "string", "description": "nome do serviço; recusado se ambíguo entre projetos" },
                    "wait": { "type": "boolean", "default": true },
                    "timeout_s": { "type": "integer", "default": 900, "maximum": 3600 },
                    "log_tail": { "type": "integer", "default": 40, "maximum": 500 }
                }
            })
        },
    },
    ToolSpec {
        name: "build_logs",
        description: "Build log de um deployment paginado por cursor. Passe o next_after \
                      da resposta anterior para pegar só as linhas novas.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "deployment_id": { "type": "string" },
                    "after": { "type": "integer", "minimum": 0, "default": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 5000, "default": 500 }
                },
                "required": ["deployment_id"]
            })
        },
    },
    ToolSpec {
        name: "ingress_routes",
        description: "Tabela de rotas VIVA do proxy reverso (domínios e portas → ip:porta). \
                      É o diagnóstico de um domínio respondendo 502.",
        input_schema: no_arguments,
    },
    ToolSpec {
        name: "ingress_reconcile",
        description: "Recalcula as rotas do proxy a partir dos containers que existem de fato, \
                      sem redeploy, e devolve a tabela já corrigida.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "service_id": { "type": "string", "description": "omitido = todos os serviços" }
                }
            })
        },
    },
    ToolSpec {
        name: "rpc",
        description: "Executa QUALQUER Command do protocolo do daemon e devolve a Response. \
                      Codificação serde externally-tagged: variante com campos é objeto de \
                      uma chave ({\"ServiceGet\":{\"id\":\"svc_…\"}}), sem campos é a string \
                      nua (\"ProjectList\"). Ver a seção de comandos do manual.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "command": { "description": "o Command em JSON, ex. \"ProjectList\" ou {\"DeployHistory\":{\"service_id\":\"svc_…\",\"limit\":3}}" }
                },
                "required": ["command"]
            })
        },
    },
];

/// Executa a ferramenta `name`.
pub async fn call(state: &AppState, name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        "manual" => Ok(json!({ "manual": super::MANUAL })),
        "status" => status(state).await,
        "list_services" => {
            let lista = service_index(&snapshot(state).await?);
            Ok(json!({ "count": lista.len(), "services": lista }))
        }
        "list_deploys" => list_deploys(state, args).await,
        "deploy" => deploy(state, args).await,
        "build_logs" => build_logs(state, args).await,
        "ingress_routes" => rpc_expect(state, json!("IngressRoutes"), "IngressRoutes").await,
        "ingress_reconcile" => {
            let service_id = args.get("service_id").and_then(Value::as_str);
            rpc_expect(
                state,
                json!({ "IngressReconcile": { "service_id": service_id } }),
                "IngressRoutes",
            )
            .await
        }
        "rpc" => {
            let command = args
                .get("command")
                .cloned()
                .ok_or_else(|| ToolError::bad_request("informe \"command\""))?;
            rpc(state, command).await
        }
        outro => Err(ToolError::new(
            "no_such_tool",
            format!("ferramenta {outro:?} não existe — veja tools/list"),
        )),
    }
}

/// Executa um `Command` (em JSON) e converte `Response::Err` em falha.
pub async fn rpc(state: &AppState, command: Value) -> Result<Value, ToolError> {
    let cmd: Command = serde_json::from_value(command)
        .map_err(|e| ToolError::bad_request(format!("Command inválido: {e}")))?;
    let resposta = serde_json::to_value(
        // No heap: o future do `dispatch` é enorme (um `match` com todo
        // handler), e aninhado no da ferramenta estourava a pilha da thread
        // tokio em build debug — `Snapshot` despacha outros comandos por dentro.
        Box::pin(dispatch(state.clone(), cmd)).await,
    )
    .map_err(|e| ToolError::new("encode_failed", e.to_string()))?;

    if let Some(err) = resposta.get("Err") {
        let code = err.get("code").and_then(Value::as_str).unwrap_or("?");
        let message = err.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(ToolError::new(
            "daemon_command_error",
            format!("{code}: {message}"),
        ));
    }
    Ok(resposta)
}

/// Como [`rpc`], mas exige a variante `variante` e devolve só o payload dela.
async fn rpc_expect(state: &AppState, command: Value, variante: &str) -> Result<Value, ToolError> {
    let resposta = rpc(state, command).await?;
    resposta.get(variante).cloned().ok_or_else(|| {
        ToolError::new(
            "unexpected_response",
            format!("esperava Response::{variante}, veio {resposta}"),
        )
    })
}

async fn status(state: &AppState) -> Result<Value, ToolError> {
    let daemon = rpc_expect(state, json!("DaemonStatus"), "DaemonStatus").await?;
    let engine = rpc_expect(state, json!("DeployEngineStatus"), "DeployEngineStatus").await?;
    Ok(json!({ "daemon": daemon, "deploy_engine": engine }))
}

/// `Snapshot` devolve JSON **dentro** de uma string.
async fn snapshot(state: &AppState) -> Result<Value, ToolError> {
    let texto = Box::pin(super::super::http_api::snapshot(state)).await;
    serde_json::from_str(&texto).map_err(|e| {
        ToolError::new(
            "unexpected_response",
            format!("snapshot não é JSON válido: {e}"),
        )
    })
}

async fn list_deploys(state: &AppState, args: &Value) -> Result<Value, ToolError> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 200);
    let sumarios = rpc_expect(
        state,
        json!({ "RecentDeployments": { "limit": limit } }),
        "DeploymentSummaries",
    )
    .await?;
    let lista: Vec<Value> = sumarios
        .as_array()
        .map(|a| a.iter().map(compact_summary).collect())
        .unwrap_or_default();
    Ok(json!({ "count": lista.len(), "deployments": lista }))
}

async fn deploy(state: &AppState, args: &Value) -> Result<Value, ToolError> {
    let service_id = resolve_service(state, args).await?;
    let esperar = args.get("wait").and_then(Value::as_bool).unwrap_or(true);
    let log_tail = args
        .get("log_tail")
        .and_then(Value::as_u64)
        .unwrap_or(40)
        .min(500) as usize;
    let limite = args
        .get("timeout_s")
        .and_then(Value::as_u64)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(900))
        .min(MAX_WAIT);

    let dep = rpc_expect(
        state,
        json!({ "DeployStart": { "service_id": service_id } }),
        "Deployment",
    )
    .await?;
    let deployment_id = dep
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    if !esperar {
        return Ok(json!({
            "deployment_id": deployment_id,
            "service_id": service_id,
            "state": dep.get("state").cloned().unwrap_or(Value::Null),
            "waited": false,
            "hint": "acompanhe com list_deploys ou build_logs(deployment_id, after)",
        }));
    }

    let (final_dep, expirou) = wait_for_outcome(state, &service_id, &deployment_id, limite).await?;

    let (log, total) = if log_tail > 0 {
        let linhas = build_log_lines(state, &deployment_id)
            .await
            .unwrap_or_default();
        let total = linhas.len();
        let inicio = total.saturating_sub(log_tail);
        (linhas[inicio..].to_vec(), total)
    } else {
        (Vec::new(), 0)
    };

    let mut out = compact_deployment(&final_dep);
    if let Value::Object(m) = &mut out {
        m.insert("waited".into(), json!(true));
        m.insert("timed_out".into(), json!(expirou));
        m.insert("log_tail".into(), json!(log));
        m.insert("log_cursor".into(), json!(total));
    }
    Ok(out)
}

/// Aceita o id direto (`service_id`) ou o nome (`service`).
async fn resolve_service(state: &AppState, args: &Value) -> Result<String, ToolError> {
    if let Some(id) = args
        .get("service_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(id.to_owned());
    }

    let nome = args
        .get("service")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::bad_request("informe \"service_id\" ou \"service\" (nome)"))?;

    let indice = service_index(&snapshot(state).await?);
    let casos: Vec<&Value> = indice
        .iter()
        .filter(|s| {
            s.get("name")
                .and_then(Value::as_str)
                .map(|n| n.eq_ignore_ascii_case(nome))
                .unwrap_or(false)
        })
        .collect();

    match casos.as_slice() {
        [um] => Ok(um
            .get("service_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()),
        [] => Err(ToolError::new(
            "no_such_service",
            format!("nenhum serviço chamado {nome:?} — veja list_services"),
        )),
        varios => Err(ToolError::bad_request(format!(
            "{:?} existe em {} projetos — mande \"service_id\". Candidatos: {}",
            nome,
            varios.len(),
            varios
                .iter()
                .filter_map(|s| {
                    Some(format!(
                        "{}={}",
                        s.get("project")?.as_str()?,
                        s.get("service_id")?.as_str()?
                    ))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Reconsulta o histórico até o deployment chegar a um estado terminal (ou o
/// prazo acabar).
async fn wait_for_outcome(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
    limite: Duration,
) -> Result<(Value, bool), ToolError> {
    let comeco = Instant::now();
    let mut ultimo: Option<Value> = None;

    loop {
        let historico = rpc_expect(
            state,
            json!({ "DeployHistory": { "service_id": service_id, "limit": 10 } }),
            "Deployments",
        )
        .await?;

        let atual = historico.as_array().and_then(|a| {
            a.iter()
                .find(|d| d.get("id").and_then(Value::as_str) == Some(deployment_id))
                .cloned()
        });

        if let Some(dep) = atual {
            if is_terminal(&dep) {
                return Ok((dep, false));
            }
            ultimo = Some(dep);
        }

        if comeco.elapsed() >= limite {
            let dep = ultimo.unwrap_or_else(
                || json!({ "id": deployment_id, "service_id": service_id, "state": "Unknown" }),
            );
            return Ok((dep, true));
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn build_logs(state: &AppState, args: &Value) -> Result<Value, ToolError> {
    let deployment_id = args
        .get("deployment_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::bad_request("informe \"deployment_id\""))?;
    let linhas = build_log_lines(state, deployment_id).await?;
    let total = linhas.len();
    let after = (args.get("after").and_then(Value::as_u64).unwrap_or(0) as usize).min(total);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(500)
        .clamp(1, 5000) as usize;
    let fim = (after + limit).min(total);

    Ok(json!({
        "deployment_id": deployment_id,
        "total": total,
        "after": after,
        "next_after": fim,
        "has_more": fim < total,
        "lines": &linhas[after..fim],
    }))
}

async fn build_log_lines(state: &AppState, deployment_id: &str) -> Result<Vec<String>, ToolError> {
    let bruto = rpc_expect(
        state,
        json!({ "GetBuildLogs": { "deployment_id": deployment_id } }),
        "BuildLogs",
    )
    .await?;
    Ok(bruto
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("line").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// `DeploymentSummary` → linha compacta com o desfecho resolvido.
fn compact_summary(sum: &Value) -> Value {
    let dep = sum.get("deployment").unwrap_or(&Value::Null);
    let mut out = compact_deployment(dep);
    if let Value::Object(m) = &mut out {
        if let Some(n) = sum.get("service_name") {
            m.insert("service".into(), n.clone());
        }
        if let Some(p) = sum.get("project_name") {
            m.insert("project".into(), p.clone());
        }
    }
    out
}

/// `Deployment` → o que interessa a quem só quer saber como acabou.
fn compact_deployment(dep: &Value) -> Value {
    let estado = dep
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("Unknown");

    json!({
        "deployment_id": dep.get("id").cloned().unwrap_or(Value::Null),
        "service_id": dep.get("service_id").cloned().unwrap_or(Value::Null),
        "image": dep.get("image").cloned().unwrap_or(Value::Null),
        "state": estado,
        "ok": outcome_ok(estado),
        "error": failure_reason(dep),
        "started_at": dep.get("started_at").cloned().unwrap_or(Value::Null),
        "finished_at": dep.get("finished_at").cloned().unwrap_or(Value::Null),
    })
}

/// `true` = no ar, `false` = falhou, `null` = ainda não decidiu. `Stopped` e
/// `Pruning` são terminais sem desfecho: chamá-los de falha seria mentira.
fn outcome_ok(estado: &str) -> Value {
    match estado {
        "Live" => json!(true),
        "Failed" => json!(false),
        _ => Value::Null,
    }
}

fn is_terminal(dep: &Value) -> bool {
    matches!(
        dep.get("state").and_then(Value::as_str),
        Some("Live" | "Stopped" | "Failed" | "Pruning")
    )
}

/// A causa da falha, tirada do `states_log`.
fn failure_reason(dep: &Value) -> Value {
    let Some(log) = dep.get("states_log").and_then(Value::as_array) else {
        return Value::Null;
    };

    log.iter()
        .rev()
        .find(|t| {
            matches!(
                t.get("to").and_then(Value::as_str),
                Some("RollingBack" | "Failed")
            ) && t.get("message").map(|m| !m.is_null()).unwrap_or(false)
        })
        .and_then(|t| t.get("message").cloned())
        .unwrap_or(Value::Null)
}

/// Achata o snapshot no índice de serviços que um agente precisa para agir.
fn service_index(snapshot: &Value) -> Vec<Value> {
    let Some(lista) = snapshot.get("services").and_then(Value::as_array) else {
        return Vec::new();
    };

    lista
        .iter()
        .filter_map(|entrada| {
            let svc = entrada.get("service")?;
            let spec = svc.get("spec")?;

            Some(json!({
                "service_id": svc.get("id").cloned().unwrap_or(Value::Null),
                "name": spec.get("name").cloned().unwrap_or(Value::Null),
                "project": entrada.get("project_name").cloned().unwrap_or(Value::Null),
                "project_id": spec.get("project_id").cloned().unwrap_or(Value::Null),
                "status": status_label(svc.get("status")),
                "source": source_label(spec.get("source")),
                "port": spec.get("port").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

/// `ServiceStatus` é externally-tagged e só `Error` tem campo — vira
/// `"Running"` ou `"Error: <causa>"`.
fn status_label(status: Option<&Value>) -> Value {
    match status {
        Some(Value::String(s)) => json!(s),
        Some(Value::Object(m)) if m.len() == 1 => {
            let (k, v) = m.iter().next().unwrap();
            match v.as_str() {
                Some(detalhe) if !detalhe.is_empty() => json!(format!("{k}: {detalhe}")),
                _ => json!(k),
            }
        }
        _ => Value::Null,
    }
}

/// `ServiceSource` achatado no que identifica a origem, sem arrastar o
/// `compose.content` inteiro (dezenas de KB) para dentro de uma listagem.
fn source_label(source: Option<&Value>) -> Value {
    match source {
        Some(Value::Object(m)) if m.len() == 1 => {
            let (kind, v) = m.iter().next().unwrap();
            match kind.as_str() {
                "Registry" => json!({
                    "kind": "Registry",
                    "image": v.get("image").cloned().unwrap_or(Value::Null),
                }),
                "Git" => json!({
                    "kind": "Git",
                    "url": v.get("url").cloned().unwrap_or(Value::Null),
                    "branch": v.get("branch").cloned().unwrap_or(Value::Null),
                    "dockerfile_path": v.get("dockerfile_path").cloned().unwrap_or(Value::Null),
                    "build_context": v.get("build_context").cloned().unwrap_or(Value::Null),
                }),
                "Archive" => json!({
                    "kind": "Archive",
                    "dockerfile_path": v.get("dockerfile_path").cloned().unwrap_or(Value::Null),
                    "original_filename": v.get("original_filename").cloned().unwrap_or(Value::Null),
                }),
                outro => json!({ "kind": outro }),
            }
        }
        Some(Value::String(s)) => json!({ "kind": s }),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desfecho_so_e_decidido_por_live_ou_failed() {
        assert_eq!(outcome_ok("Live"), json!(true));
        assert_eq!(outcome_ok("Failed"), json!(false));
        assert_eq!(outcome_ok("Stopped"), Value::Null);
        assert_eq!(outcome_ok("Pruning"), Value::Null);
        assert_eq!(outcome_ok("BuildingImage"), Value::Null);
    }

    #[test]
    fn causa_da_falha_vem_da_ultima_transicao_com_mensagem() {
        let dep = json!({
            "state": "Failed",
            "states_log": [
                { "to": "BuildingImage", "message": null },
                { "to": "RollingBack", "message": "docker build error: sem Dockerfile" },
                { "to": "Failed", "message": null }
            ]
        });
        assert_eq!(
            failure_reason(&dep),
            json!("docker build error: sem Dockerfile")
        );
    }

    #[test]
    fn status_externally_tagged_vira_texto() {
        assert_eq!(status_label(Some(&json!("Running"))), json!("Running"));
        assert_eq!(
            status_label(Some(&json!({ "Error": "porta ocupada" }))),
            json!("Error: porta ocupada")
        );
    }

    #[test]
    fn todas_as_ferramentas_tem_schema_de_objeto() {
        for tool in TOOLS {
            assert_eq!(
                (tool.input_schema)()["type"],
                json!("object"),
                "{}",
                tool.name
            );
        }
    }
}
