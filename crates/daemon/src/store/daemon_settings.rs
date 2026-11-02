//! Tabela chave-valor de configurações do daemon editáveis pela UI (ACME,
//! registry, limpeza do Docker).

use anyhow::Result;

use super::Db;

/// As três chaves de **configuração** moram no YAML de infra (`settings.yml`); as demais
/// (`*_last_run`, `job_warm:*`, `service_warm:*`…) são runtime e continuam no SQL.
/// Plano: `docs/plano-backup-estado-em-git.md`, R2.1.
fn is_infra_key(key: &str) -> bool {
    matches!(
        key,
        KEY_ACME_EMAIL | KEY_REGISTRY_DOMAIN | KEY_DOCKER_CLEANUP_CONFIG
    )
}

pub async fn get(db: &Db, key: &str) -> Result<Option<String>> {
    if is_infra_key(key) {
        let st = db.infra().snapshot();
        return Ok(match key {
            KEY_ACME_EMAIL => st.settings.acme_email.clone(),
            KEY_REGISTRY_DOMAIN => st.settings.registry_domain.clone(),
            _ => st.settings.docker_cleanup_config.as_ref().map(|v| match v {
                // o valor que não era JSON válido foi guardado como texto: devolve como veio
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
        });
    }
    sql_get(db, key).await
}

pub async fn set(db: &Db, key: &str, value: &str) -> Result<()> {
    if is_infra_key(key) {
        let value = value.to_string();
        return db.infra().update(|st| {
            match key {
                KEY_ACME_EMAIL => st.settings.acme_email = Some(value),
                KEY_REGISTRY_DOMAIN => st.settings.registry_domain = Some(value),
                _ => {
                    st.settings.docker_cleanup_config = Some(
                        serde_json::from_str(&value)
                            .unwrap_or(serde_json::Value::String(value.clone())),
                    )
                }
            }
            Ok(())
        });
    }
    sqlx::query(
        "INSERT INTO daemon_settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn delete(db: &Db, key: &str) -> Result<()> {
    if is_infra_key(key) {
        return db.infra().update(|st| {
            match key {
                KEY_ACME_EMAIL => st.settings.acme_email = None,
                KEY_REGISTRY_DOMAIN => st.settings.registry_domain = None,
                _ => st.settings.docker_cleanup_config = None,
            }
            Ok(())
        });
    }
    sqlx::query("DELETE FROM daemon_settings WHERE key = ?")
        .bind(key)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Leitura **direta da tabela**, sem desviar para o YAML. Usada pela migração SQL → YAML e pelas
/// chaves de runtime.
pub async fn sql_get(db: &Db, key: &str) -> Result<Option<String>> {
    let row = sqlx::query_scalar::<_, String>("SELECT value FROM daemon_settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db.pool())
        .await?;
    Ok(row)
}

/// DEPRECADA: a base pública passou a ser derivada de `[api] domain`/`port`
/// (ver `AppState::public_base_url`), então esta chave não é mais lida nem
/// escrita. Linhas remanescentes em bancos de instalações antigas são inertes.
#[allow(dead_code)]
pub const KEY_WEBHOOK_BASE_URL: &str = "webhook_base_url";
pub const KEY_ACME_EMAIL: &str = "acme_email";
pub const KEY_REGISTRY_DOMAIN: &str = "registry_domain";
/// JSON de `shared::DockerCleanupConfig` — ver `crate::features::maintenance`.
pub const KEY_DOCKER_CLEANUP_CONFIG: &str = "docker_cleanup_config";
/// JSON de `shared::DockerCleanupLastRun`, ausente até a primeira execução.
pub const KEY_DOCKER_CLEANUP_LAST_RUN: &str = "docker_cleanup_last_run";
/// JSON de `BuildCachePrune`: quando o cache de build foi limpo pela última vez
/// e com qual corte de idade.
pub const KEY_BUILD_CACHE_LAST_PRUNE: &str = "build_cache_last_prune";
/// Prefixo das chaves `job_warm:<job_id>` (JSON de `jobs::warm::WarmRecord`).
pub const KEY_JOB_WARM_PREFIX: &str = "job_warm:";
/// Prefixo das chaves `service_warm:<service_id>` (JSON de
/// `maintenance::service_warm::WarmRecord`).
pub const KEY_SERVICE_WARM_PREFIX: &str = "service_warm:";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chaves_de_config_vao_ao_yaml_e_as_de_runtime_ficam_no_sql() {
        let db = Db::memory_for_test().await;
        set(&db, KEY_ACME_EMAIL, "a@b.c").await.unwrap();
        set(&db, KEY_DOCKER_CLEANUP_CONFIG, r#"{"enabled":true}"#)
            .await
            .unwrap();
        set(&db, KEY_DOCKER_CLEANUP_LAST_RUN, "{}").await.unwrap();

        assert_eq!(
            get(&db, KEY_ACME_EMAIL).await.unwrap().as_deref(),
            Some("a@b.c")
        );
        assert_eq!(
            get(&db, KEY_DOCKER_CLEANUP_CONFIG)
                .await
                .unwrap()
                .as_deref(),
            Some(r#"{"enabled":true}"#)
        );
        // config está no YAML e não na tabela; runtime é o contrário
        assert!(sql_get(&db, KEY_ACME_EMAIL).await.unwrap().is_none());
        assert!(
            sql_get(&db, KEY_DOCKER_CLEANUP_LAST_RUN)
                .await
                .unwrap()
                .is_some()
        );
        let yaml =
            String::from_utf8(db.infra().tree().read("settings.yml").unwrap().unwrap()).unwrap();
        assert!(yaml.contains("a@b.c") && !yaml.contains("last_run"));

        delete(&db, KEY_ACME_EMAIL).await.unwrap();
        assert!(get(&db, KEY_ACME_EMAIL).await.unwrap().is_none());
    }
}
