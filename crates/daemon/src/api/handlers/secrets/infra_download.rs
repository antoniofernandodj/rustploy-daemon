//! `Command::InfraDownload`: a infra como os arquivos que o `state_git` grava no repositório.
//!
//! É a mesma árvore (`InfraState::to_files`), com os segredos como `ENC[age:…]`: serve de cópia
//! de segurança fora do GitHub e para inspeção, e restaurar é pôr a árvore num repositório e
//! sincronizar. Recusa quando o git também recusaria o commit (valor em claro): o download não
//! pode vazar o que o git não aceitaria.

use crate::bootstrap::state::AppState;
use crate::features::state_git::blocking;
use shared::{InfraFile, Response};

pub async fn handle(state: AppState) -> Response {
    let snapshot = state.db.infra().snapshot();
    let allowed = state.state_git.allowed().await;
    let bloqueios = blocking(&snapshot, &allowed);
    if !bloqueios.is_empty() {
        return Response::err(
            "PlaintextInInfra",
            format!(
                "há {} valor(es) em claro no estado de infra; mova-os para secrets (veja InfraAudit) antes de baixar a infra",
                bloqueios.len()
            ),
        );
    }
    let files = match snapshot.to_files() {
        Ok(f) => f,
        Err(e) => return Response::err("InfraError", e.to_string()),
    };
    let mut out = Vec::with_capacity(files.len());
    for (path, bytes) in files {
        match String::from_utf8(bytes) {
            Ok(content) => out.push(InfraFile { path, content }),
            Err(_) => {
                return Response::err("InfraError", format!("{path}: conteúdo não é texto UTF-8"));
            }
        }
    }
    Response::InfraFiles(out)
}
