//! `Command::ServiceDescriptionSet`: grava só a descrição do serviço, sem reenviar o spec. Não muda o
//! que roda (nada de redeploy). Ver `docs/arquivo/plano-descricao-do-servico.md`.

use crate::domain::Service;
use crate::store::Db;
use anyhow::{Result, anyhow};

/// Troca a descrição (texto vazio apaga) e devolve o serviço gravado.
pub async fn set(db: &Db, service_id: &str, description: &str) -> Result<Service> {
    let description =
        crate::domain::normalize_description(Some(description)).map_err(|e| anyhow!(e))?;
    let service = crate::store::services::get(db, service_id)
        .await?
        .ok_or_else(|| anyhow!("service not found"))?;
    if service.spec.description == description {
        return Ok(service);
    }
    let mut spec = service.spec;
    spec.description = description;
    crate::store::services::update_spec(db, service_id, spec)
        .await?
        .ok_or_else(|| anyhow!("service not found"))
}
