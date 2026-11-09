//! Exportar e importar os **dados** de um serviço (volumes). Tudo parte de um `service_id`: o
//! daemon descobre sozinho quais volumes o serviço tem (ver `parts`). Plano e decisões em
//! `docs/plano-exportar-importar-dados-de-servico.md`.
//!
//! - [`plan`] só olha (`Command::ServiceDataPlan`);
//! - [`transfer::export`] e [`transfer::import`] movem os bytes (rotas HTTP binárias).

pub mod database;
pub mod pack;
pub mod parts;
pub mod stream;
pub mod transfer;

use crate::features::deploy::DeployContext;
use shared::{DataPart, Response as RpResponse, ServiceDataPlan, ServiceStatus};

/// Imagem descartável que monta o volume só para ler/gravar o tar.
pub const HELPER_IMAGE: &str = "docker.io/library/busybox:stable";

pub fn is_running(status: &ServiceStatus) -> bool {
    matches!(status, ServiceStatus::Running | ServiceStatus::Degraded)
}

/// `Command::ServiceDataPlan`: o que um export levaria. Não muda nada.
pub async fn plan(state: DeployContext, service_id: String) -> RpResponse {
    let svc = match crate::store::services::get(&state.db, &service_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return RpResponse::err("NotFound", "serviço não encontrado"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    let parts = match parts::resolve(&state.docker, &svc).await {
        Ok(p) => p,
        Err(e) => {
            // serviço de banco parado etc.: a tela mostra a causa no lugar do botão
            return RpResponse::ServiceDataPlan(ServiceDataPlan {
                service_name: svc.spec.name.clone(),
                parts: vec![],
                running: is_running(&svc.status),
                blocked: Some(e.to_string()),
                notes: vec![],
                encrypt_default: false,
            });
        }
    };
    let running = is_running(&svc.status);
    let blocked = parts
        .is_empty()
        .then(|| "Este serviço não tem volumes: não há dados para exportar.".to_string());
    let mut notes = Vec::new();
    if parts
        .iter()
        .any(|p| matches!(p.source, parts::Source::Database(_)))
    {
        notes.push(
            "Banco: o dump lógico é feito com o serviço no ar e não o interrompe. Ao importar, o \
             conteúdo do banco é substituído sem parar o serviço: pare antes quem o usa."
                .to_string(),
        );
    } else if running && !parts.is_empty() {
        notes.push(
            "O serviço está no ar. Exportar com `stop=true` o para durante a cópia e o religa \
             depois; sem parar, a cópia pode sair inconsistente (nunca use assim num banco)."
                .to_string(),
        );
    }
    RpResponse::ServiceDataPlan(ServiceDataPlan {
        service_name: svc.spec.name.clone(),
        parts: parts
            .iter()
            .map(|p| DataPart {
                name: p.name.clone(),
                kind: if matches!(p.source, parts::Source::Database(_)) {
                    "database"
                } else {
                    "volume"
                }
                .into(),
                size_bytes: None,
            })
            .collect(),
        running,
        blocked,
        notes,
        encrypt_default: state.secrets.has_recovery(),
    })
}
