//! Persistence for connected Git providers (Gitea OAuth2 / PAT).
//!
//! Secrets (OAuth client secret, access/refresh tokens, PAT) are stored already
//! encrypted by the caller via `SecretsManager`; this module only moves opaque
//! strings in and out of SQLite. The public [`shared::GitProvider`] projection
//! deliberately omits every secret column.

use super::Db;
use anyhow::Result;
use chrono::{DateTime, Utc};
use shared::{GitAccount, GitAuthMode, GitProvider, GitProviderKind};

/// Full row as stored, including the encrypted secret columns. Used internally
/// by the daemon (e.g. the deploy executor and OAuth callback); never sent to
/// clients — call [`StoredProvider::to_public`] for that.
#[derive(Debug, Clone)]
pub struct StoredProvider {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub base_url: String,
    pub auth_mode: String,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret_enc: Option<String>,
    pub access_token_enc: Option<String>,
    pub refresh_token_enc: Option<String>,
    pub account_login: Option<String>,
    pub account_avatar: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl StoredProvider {
    /// Client-facing projection. Drops every secret column.
    pub fn to_public(&self) -> GitProvider {
        let account = self.account_login.as_ref().map(|login| GitAccount {
            login: login.clone(),
            avatar_url: self.account_avatar.clone(),
        });
        GitProvider {
            id: self.id.clone(),
            kind: GitProviderKind::from_str(&self.kind).unwrap_or(GitProviderKind::Gitea),
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            auth_mode: GitAuthMode::from_str(&self.auth_mode).unwrap_or(GitAuthMode::OAuth),
            oauth_client_id: self.oauth_client_id.clone(),
            account,
            created_at: self.created_at,
        }
    }
}

// A **configuração** do provider mora no YAML de infra (`providers.yml`); os **tokens de login**
// (`access_token_enc`/`refresh_token_enc`) ficam só no SQL, na tabela `git_provider_token`, e nunca
// vão ao git (plano `docs/plano-backup-estado-em-git.md`, R9.0 item 1). A tabela antiga
// `git_provider` não é mais lida nem escrita (fica como rollback).

use crate::store::infra::model::ProviderInfra;
use std::collections::BTreeMap;

type Tokens = (Option<String>, Option<String>);

async fn all_tokens(db: &Db) -> Result<BTreeMap<String, Tokens>> {
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT provider_id, access_token_enc, refresh_token_enc FROM git_provider_token",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows.into_iter().map(|(i, a, r)| (i, (a, r))).collect())
}

fn stored(p: &ProviderInfra, tokens: Tokens) -> StoredProvider {
    StoredProvider {
        id: p.id.clone(),
        kind: p.kind.clone(),
        name: p.name.clone(),
        base_url: p.base_url.clone(),
        auth_mode: p.auth_mode.clone(),
        oauth_client_id: p.oauth_client_id.clone(),
        oauth_client_secret_enc: p.oauth_client_secret_enc.clone(),
        access_token_enc: tokens.0,
        refresh_token_enc: tokens.1,
        account_login: p.account_login.clone(),
        account_avatar: p.account_avatar.clone(),
        created_at: p.created_at,
    }
}

async fn put_tokens(db: &Db, id: &str, access: Option<&str>, refresh: Option<&str>) -> Result<()> {
    sqlx::query(
        "INSERT INTO git_provider_token (provider_id, access_token_enc, refresh_token_enc) \
         VALUES (?, ?, ?) ON CONFLICT(provider_id) DO UPDATE SET \
         access_token_enc = excluded.access_token_enc, refresh_token_enc = excluded.refresh_token_enc",
    )
    .bind(id)
    .bind(access)
    .bind(refresh)
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn insert(db: &Db, p: &StoredProvider) -> Result<()> {
    let rec = ProviderInfra {
        id: p.id.clone(),
        kind: p.kind.clone(),
        name: p.name.clone(),
        base_url: p.base_url.clone(),
        auth_mode: p.auth_mode.clone(),
        oauth_client_id: p.oauth_client_id.clone(),
        oauth_client_secret_enc: p.oauth_client_secret_enc.clone(),
        account_login: p.account_login.clone(),
        account_avatar: p.account_avatar.clone(),
        created_at: p.created_at,
    };
    db.infra().update(|st| {
        if st.providers.iter().any(|x| x.id == rec.id) {
            anyhow::bail!("já existe um provider com o id {}", rec.id);
        }
        st.providers.push(rec);
        Ok(())
    })?;
    if p.access_token_enc.is_some() || p.refresh_token_enc.is_some() {
        put_tokens(
            db,
            &p.id,
            p.access_token_enc.as_deref(),
            p.refresh_token_enc.as_deref(),
        )
        .await?;
    }
    Ok(())
}

pub async fn list(db: &Db) -> Result<Vec<StoredProvider>> {
    let tokens = all_tokens(db).await?;
    let st = db.infra().snapshot();
    Ok(st
        .providers
        .iter()
        .map(|p| stored(p, tokens.get(&p.id).cloned().unwrap_or_default()))
        .collect())
}

pub async fn get(db: &Db, id: &str) -> Result<Option<StoredProvider>> {
    let st = db.infra().snapshot();
    let Some(p) = st.providers.iter().find(|p| p.id == id) else {
        return Ok(None);
    };
    let tokens = all_tokens(db).await?.remove(id).unwrap_or_default();
    Ok(Some(stored(p, tokens)))
}

/// Records the connected account and its tokens once OAuth completes (or a PAT
/// validates). Tokens come in already encrypted. A conta (login/avatar) vai ao YAML; os tokens, ao SQL.
pub async fn set_tokens(
    db: &Db,
    id: &str,
    access_token_enc: Option<&str>,
    refresh_token_enc: Option<&str>,
    account_login: &str,
    account_avatar: Option<&str>,
) -> Result<()> {
    let found = db
        .infra()
        .update(|st| match st.providers.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.account_login = Some(account_login.to_string());
                p.account_avatar = account_avatar.map(str::to_string);
                Ok(true)
            }
            None => Ok(false),
        })?;
    if found {
        put_tokens(db, id, access_token_enc, refresh_token_enc).await?;
    }
    Ok(())
}

pub async fn delete(db: &Db, id: &str) -> Result<bool> {
    let removed = db.infra().update(|st| {
        let before = st.providers.len();
        st.providers.retain(|p| p.id != id);
        Ok(st.providers.len() != before)
    })?;
    sqlx::query("DELETE FROM git_provider_token WHERE provider_id = ?")
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(id: &str) -> StoredProvider {
        StoredProvider {
            id: id.into(),
            kind: "GitHub".into(),
            name: "gh".into(),
            base_url: "https://github.com".into(),
            auth_mode: "OAuth".into(),
            oauth_client_id: Some("cid".into()),
            oauth_client_secret_enc: Some("seg".into()),
            access_token_enc: Some("TOK".into()),
            refresh_token_enc: None,
            account_login: None,
            account_avatar: None,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn config_no_yaml_tokens_so_no_sql() {
        let db = Db::memory_for_test().await;
        insert(&db, &prov("g1")).await.unwrap();
        assert!(insert(&db, &prov("g1")).await.is_err(), "id repetido");

        let got = get(&db, "g1").await.unwrap().unwrap();
        assert_eq!(got.access_token_enc.as_deref(), Some("TOK"));
        assert_eq!(list(&db).await.unwrap().len(), 1);

        let yaml =
            String::from_utf8(db.infra().tree().read("providers.yml").unwrap().unwrap()).unwrap();
        assert!(yaml.contains("seg") && !yaml.contains("TOK"));

        set_tokens(&db, "g1", Some("NOVO"), Some("REF"), "eu", Some("av"))
            .await
            .unwrap();
        let got = get(&db, "g1").await.unwrap().unwrap();
        assert_eq!(
            (
                got.access_token_enc.as_deref(),
                got.refresh_token_enc.as_deref()
            ),
            (Some("NOVO"), Some("REF"))
        );
        assert_eq!(got.account_login.as_deref(), Some("eu"));
        let yaml =
            String::from_utf8(db.infra().tree().read("providers.yml").unwrap().unwrap()).unwrap();
        assert!(yaml.contains("account_login: eu") && !yaml.contains("NOVO"));

        // set_tokens num provider que não existe é inofensivo
        set_tokens(&db, "fantasma", Some("x"), None, "y", None)
            .await
            .unwrap();
        assert!(get(&db, "fantasma").await.unwrap().is_none());

        assert!(delete(&db, "g1").await.unwrap());
        assert!(!delete(&db, "g1").await.unwrap());
        assert!(get(&db, "g1").await.unwrap().is_none());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM git_provider_token")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
}
