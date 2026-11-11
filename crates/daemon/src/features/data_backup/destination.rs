//! O destino S3 dos backups. Mora em `settings.yml` (vai para o `state_git`) como JSON em
//! `daemon_settings[backup_destination]`; a chave secreta fica cifrada (`ENC[age:…]`, para a identidade
//! do servidor **e** a de recuperação) e **nunca** sai do daemon: a UI só recebe `secret_set`.
//!
//! Depois de restaurar um servidor a partir do git, o destino já vem junto e a chave abre com a chave
//! de recuperação, então os backups continuam sozinhos.

use crate::platform::age_format::is_enc;
use crate::platform::s3::S3Config;
use crate::platform::secrets::SecretsManager;
use crate::store::Db;
use crate::store::daemon_settings::{self, KEY_BACKUP_DESTINATION};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use shared::{BackupDestination, BackupDestinationView};

/// Como o destino é gravado.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Stored {
    endpoint: String,
    region: String,
    bucket: String,
    #[serde(default)]
    prefix: String,
    #[serde(default = "yes")]
    path_style: bool,
    access_key: String,
    /// `ENC[age:…]`.
    secret_key: String,
}

fn yes() -> bool {
    true
}

async fn load(db: &Db) -> Option<Stored> {
    let raw = daemon_settings::get(db, KEY_BACKUP_DESTINATION)
        .await
        .ok()??;
    serde_json::from_str(&raw).ok()
}

/// O destino gravado, sem a chave secreta.
pub async fn view(db: &Db, secrets: &SecretsManager) -> BackupDestinationView {
    let recovery_configured = secrets.has_recovery();
    match load(db).await {
        Some(s) => BackupDestinationView {
            configured: true,
            endpoint: s.endpoint,
            region: s.region,
            bucket: s.bucket,
            prefix: s.prefix,
            path_style: s.path_style,
            access_key: s.access_key,
            secret_set: is_enc(&s.secret_key),
            recovery_configured,
        },
        None => BackupDestinationView {
            recovery_configured,
            ..Default::default()
        },
    }
}

/// Apara o que a pessoa digitou (espaços, `/` sobrando no endpoint e no prefixo).
fn normalize(mut d: BackupDestination) -> BackupDestination {
    d.endpoint = d.endpoint.trim().trim_end_matches('/').to_string();
    d.region = d.region.trim().to_string();
    d.bucket = d.bucket.trim().to_string();
    d.prefix = d.prefix.trim().trim_matches('/').to_string();
    d.access_key = d.access_key.trim().to_string();
    d
}

/// Une o rascunho da tela com o gravado: `secret_key: None` usa a chave que já está no servidor.
fn to_config(db_secret: Option<String>, d: &BackupDestination) -> Result<S3Config> {
    let secret_key = match (&d.secret_key, db_secret) {
        (Some(s), _) if !s.is_empty() => s.clone(),
        (_, Some(saved)) => saved,
        _ => bail!("informe a chave secreta"),
    };
    let cfg = S3Config {
        endpoint: d.endpoint.clone(),
        region: d.region.clone(),
        bucket: d.bucket.clone(),
        access_key: d.access_key.clone(),
        secret_key,
        path_style: d.path_style,
    };
    cfg.validate()?;
    Ok(cfg)
}

/// A chave secreta gravada, já decifrada.
async fn saved_secret(db: &Db, secrets: &SecretsManager) -> Result<Option<String>> {
    match load(db).await {
        Some(s) => Ok(Some(secrets.decrypt(&s.secret_key).map_err(|e| {
            anyhow!("não consegui abrir a chave secreta gravada: {e}")
        })?)),
        None => Ok(None),
    }
}

/// Grava o destino. Confere os campos, mas não conecta.
pub async fn save(
    db: &Db,
    secrets: &SecretsManager,
    dest: BackupDestination,
) -> Result<BackupDestinationView> {
    let dest = normalize(dest);
    let saved = saved_secret(db, secrets).await.ok().flatten();
    let cfg = to_config(saved, &dest)?;
    let stored = Stored {
        endpoint: dest.endpoint,
        region: dest.region,
        bucket: dest.bucket,
        prefix: dest.prefix,
        path_style: dest.path_style,
        access_key: dest.access_key,
        secret_key: secrets.encrypt(&cfg.secret_key)?,
    };
    daemon_settings::set(db, KEY_BACKUP_DESTINATION, &serde_json::to_string(&stored)?).await?;
    Ok(view(db, secrets).await)
}

pub async fn clear(db: &Db) -> Result<()> {
    daemon_settings::delete(db, KEY_BACKUP_DESTINATION).await
}

/// O destino pronto para usar: o cliente S3 e o prefixo. `draft` (rascunho da tela) tem prioridade
/// sobre o gravado; sem rascunho e sem gravado, erro explicando.
pub async fn resolve(
    db: &Db,
    secrets: &SecretsManager,
    draft: Option<BackupDestination>,
) -> Result<(S3Config, String)> {
    let saved = saved_secret(db, secrets).await;
    match draft {
        Some(d) => {
            let d = normalize(d);
            let cfg = to_config(saved.ok().flatten(), &d)?;
            Ok((cfg, d.prefix))
        }
        None => {
            let secret = saved?.ok_or_else(|| anyhow!("nenhum destino S3 configurado"))?;
            let s = load(db)
                .await
                .ok_or_else(|| anyhow!("nenhum destino S3 configurado"))?;
            let cfg = S3Config {
                endpoint: s.endpoint,
                region: s.region,
                bucket: s.bucket,
                access_key: s.access_key,
                secret_key: secret,
                path_style: s.path_style,
            };
            cfg.validate()?;
            Ok((cfg, s.prefix))
        }
    }
}

/// **Testar conexão**: grava, lê e apaga um objeto pequeno.
pub async fn test(
    db: &Db,
    secrets: &SecretsManager,
    draft: Option<BackupDestination>,
) -> Result<()> {
    let (cfg, prefix) = resolve(db, secrets, draft).await?;
    crate::platform::s3::test(&cfg, &prefix).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(secret: Option<&str>) -> BackupDestination {
        BackupDestination {
            endpoint: " https://s3.example.com/ ".into(),
            region: "us-east-1".into(),
            bucket: "bk".into(),
            prefix: "/rustploy/chiquitos/".into(),
            path_style: true,
            access_key: "AK".into(),
            secret_key: secret.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn grava_cifrado_e_nunca_devolve_a_chave() {
        let db = std::sync::Arc::new(Db::memory_for_test().await);
        let secrets = SecretsManager::new(db.clone()).unwrap();
        assert!(!view(&db, &secrets).await.configured);

        let v = save(&db, &secrets, dest(Some("segredo-de-verdade")))
            .await
            .unwrap();
        assert!(v.configured && v.secret_set);
        assert_eq!(v.endpoint, "https://s3.example.com");
        assert_eq!(v.prefix, "rustploy/chiquitos");

        // no armazenamento a chave está cifrada, não em claro
        let raw = daemon_settings::get(&db, KEY_BACKUP_DESTINATION)
            .await
            .unwrap()
            .unwrap();
        assert!(!raw.contains("segredo-de-verdade"), "{raw}");
        assert!(raw.contains("ENC[age:"), "{raw}");
        // e a resposta para a UI também não a traz
        assert!(!serde_json::to_string(&v).unwrap().contains("segredo"));
    }

    #[tokio::test]
    async fn salvar_sem_chave_mantem_a_gravada_e_recusa_se_nao_ha() {
        let db = std::sync::Arc::new(Db::memory_for_test().await);
        let secrets = SecretsManager::new(db.clone()).unwrap();
        assert!(save(&db, &secrets, dest(None)).await.is_err());

        save(&db, &secrets, dest(Some("s1"))).await.unwrap();
        let mut d = dest(None);
        d.bucket = "outro".into();
        let v = save(&db, &secrets, d).await.unwrap();
        assert_eq!(v.bucket, "outro");
        let (cfg, prefix) = resolve(&db, &secrets, None).await.unwrap();
        assert_eq!(
            (cfg.secret_key.as_str(), prefix.as_str()),
            ("s1", "rustploy/chiquitos")
        );
    }

    #[tokio::test]
    async fn rascunho_usa_a_chave_gravada_e_limpar_apaga() {
        let db = std::sync::Arc::new(Db::memory_for_test().await);
        let secrets = SecretsManager::new(db.clone()).unwrap();
        save(&db, &secrets, dest(Some("s1"))).await.unwrap();
        let (cfg, _) = resolve(&db, &secrets, Some(dest(None))).await.unwrap();
        assert_eq!(cfg.secret_key, "s1");
        let (cfg, _) = resolve(&db, &secrets, Some(dest(Some("nova"))))
            .await
            .unwrap();
        assert_eq!(cfg.secret_key, "nova");

        clear(&db).await.unwrap();
        assert!(!view(&db, &secrets).await.configured);
        assert!(resolve(&db, &secrets, None).await.is_err());
    }

    #[tokio::test]
    async fn recusa_destino_invalido() {
        let db = std::sync::Arc::new(Db::memory_for_test().await);
        let secrets = SecretsManager::new(db.clone()).unwrap();
        let mut d = dest(Some("s"));
        d.endpoint = "ftp://x".into();
        assert!(save(&db, &secrets, d).await.is_err());
        let mut d = dest(Some("s"));
        d.bucket = "".into();
        assert!(save(&db, &secrets, d).await.is_err());
    }
}
