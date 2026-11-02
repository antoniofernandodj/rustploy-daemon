//! Tabela de tokens de webhook de deploy, um por serviço.

use anyhow::Result;

use super::Db;

// O segredo do webhook mora no YAML, junto do serviço (`ServiceInfra.webhook_token`), **cifrado**
// (`ENC[age:…]`, Fase 5). Um valor ainda em claro (vindo da migração SQL → YAML) é lido como está
// e regravado cifrado por `SecretsManager::reencrypt_legacy`. A tabela antiga `webhook_token` não
// é mais lida nem escrita.

use crate::platform::secrets::{SecretsManager, is_enc};

fn service_mut<'a>(
    st: &'a mut super::infra::model::InfraState,
    service_id: &str,
) -> Option<&'a mut super::infra::model::ServiceInfra> {
    st.projects
        .iter_mut()
        .flat_map(|p| p.services.iter_mut())
        .find(|s| s.id == service_id)
}

pub async fn get(db: &Db, secrets: &SecretsManager, service_id: &str) -> Result<Option<String>> {
    let stored = db
        .infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
        .find(|s| s.id == service_id)
        .and_then(|s| s.webhook_token.clone());
    match stored {
        Some(v) if is_enc(&v) => Ok(Some(secrets.decrypt(&v)?)),
        other => Ok(other),
    }
}

pub async fn upsert(
    db: &Db,
    secrets: &SecretsManager,
    service_id: &str,
    token: &str,
) -> Result<()> {
    let enc = secrets.encrypt(token)?;
    db.infra().update(|st| {
        match service_mut(st, service_id) {
            Some(s) => s.webhook_token = Some(enc),
            None => anyhow::bail!("serviço {service_id} não encontrado"),
        }
        Ok(())
    })
}

pub async fn _delete(db: &Db, service_id: &str) -> Result<()> {
    db.infra().update(|st| {
        if let Some(s) = service_mut(st, service_id) {
            s.webhook_token = None;
        }
        Ok(())
    })
}

pub fn generate_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes).map(|_| ()))
        .unwrap_or_default();
    hex::encode(bytes)
}
