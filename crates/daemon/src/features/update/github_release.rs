//! A última release estável de um repositório do GitHub (`GET /repos/{repo}/releases/latest`).
//! Esse endpoint nunca devolve rascunho nem pré-lançamento, que é o canal "estável" que queremos.

use crate::platform::https_client;
use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// O JSON de uma release cabe em poucos KB; o teto só protege de uma resposta absurda.
const MAX_BODY_BYTES: usize = 512 * 1024;

/// Um arquivo anexado à release (`rustployd_0.1.10-1_amd64.deb`, o `.minisig`…).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRelease {
    /// A tag como publicada (`v0.1.7`).
    pub tag: String,
    pub url: String,
    pub published_at: Option<DateTime<Utc>>,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleasePayload {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    assets: Vec<AssetPayload>,
}

#[derive(Deserialize)]
struct AssetPayload {
    name: String,
    browser_download_url: String,
}

/// `dono/nome`, só com os caracteres que o GitHub aceita. Vem da config, mas vira
/// parte de uma URL, então não se confia nela às cegas.
pub fn is_valid_repo(repo: &str) -> bool {
    let valid_part = |part: &str| {
        // `.` e `..` só têm caracteres permitidos mas viram navegação no caminho da URL.
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    };
    match repo.split_once('/') {
        Some((owner, name)) => valid_part(owner) && valid_part(name),
        None => false,
    }
}

/// Versão que recebe a `String` por valor, para o `future` não emprestar nada do chamador.
pub async fn fetch_latest_owned(repo: String) -> Result<LatestRelease> {
    fetch_latest(&repo).await
}

pub async fn fetch_latest(repo: &str) -> Result<LatestRelease> {
    if !is_valid_repo(repo) {
        bail!("repositório inválido em [update] repo: {repo:?} (esperado dono/nome)");
    }
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let user_agent = format!("rustployd/{}", env!("CARGO_PKG_VERSION"));
    let response = https_client::get(
        &url,
        &[
            ("user-agent", user_agent.as_str()),
            ("accept", "application/vnd.github+json"),
            ("x-github-api-version", "2022-11-28"),
        ],
        REQUEST_TIMEOUT,
        MAX_BODY_BYTES,
    )
    .await
    .map_err(|error| anyhow!("não consegui consultar o GitHub: {error}"))?;

    parse_response(
        response.status,
        response.header("x-ratelimit-remaining"),
        &response.body,
    )
}

fn parse_response(
    status: u16,
    rate_limit_remaining: Option<&str>,
    body: &[u8],
) -> Result<LatestRelease> {
    match status {
        200 => {
            let payload: ReleasePayload = serde_json::from_slice(body)
                .map_err(|error| anyhow!("resposta do GitHub ilegível: {error}"))?;
            Ok(LatestRelease {
                tag: payload.tag_name,
                url: payload.html_url,
                published_at: payload.published_at,
                assets: payload
                    .assets
                    .into_iter()
                    .map(|asset| ReleaseAsset {
                        name: asset.name,
                        url: asset.browser_download_url,
                    })
                    .collect(),
            })
        }
        404 => bail!("o repositório não tem nenhuma release publicada (ou não é público)"),
        // 403 com a cota zerada é o limite da API anônima; 429 é o limite secundário.
        429 => bail!("limite de consultas do GitHub atingido; tente de novo daqui a pouco"),
        403 if rate_limit_remaining == Some("0") => {
            bail!("limite de consultas do GitHub atingido; tente de novo daqui a pouco")
        }
        other => bail!("o GitHub respondeu HTTP {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "tag_name": "v0.1.8",
        "html_url": "https://github.com/antoniofernandodj/rustploy/releases/tag/v0.1.8",
        "published_at": "2026-10-20T15:04:05Z",
        "draft": false,
        "prerelease": false,
        "assets": [
            {"name": "rustployd_0.1.10-1_amd64.deb", "size": 6653000,
             "browser_download_url": "https://github.com/o/r/releases/download/v0.1.8/rustployd_0.1.10-1_amd64.deb"},
            {"name": "checksums.sha256", "browser_download_url": "https://github.com/o/r/releases/download/v0.1.8/checksums.sha256"}
        ]
    }"#;

    #[test]
    fn parses_a_release() {
        let release = parse_response(200, Some("59"), SAMPLE.as_bytes()).unwrap();
        assert_eq!(release.tag, "v0.1.8");
        assert!(release.url.ends_with("/releases/tag/v0.1.8"));
        assert_eq!(release.assets.len(), 2);
        assert_eq!(release.assets[0].name, "rustployd_0.1.10-1_amd64.deb");
        assert!(
            release.assets[0]
                .url
                .starts_with("https://github.com/o/r/releases/download/")
        );
        assert_eq!(
            release.published_at.unwrap().to_rfc3339(),
            "2026-10-20T15:04:05+00:00"
        );
    }

    #[test]
    fn published_at_is_optional() {
        let body = r#"{"tag_name":"v1.0.0","html_url":"https://x"}"#;
        assert_eq!(
            parse_response(200, None, body.as_bytes())
                .unwrap()
                .published_at,
            None
        );
    }

    #[test]
    fn missing_release_is_a_clear_error() {
        let error = parse_response(404, None, b"{}").unwrap_err().to_string();
        assert!(error.contains("nenhuma release"), "{error}");
    }

    #[test]
    fn rate_limit_is_recognised_both_ways() {
        for (status, remaining) in [(403, Some("0")), (429, None)] {
            let error = parse_response(status, remaining, b"{}")
                .unwrap_err()
                .to_string();
            assert!(error.contains("limite de consultas"), "{error}");
        }
    }

    #[test]
    fn plain_403_is_not_called_a_rate_limit() {
        let error = parse_response(403, Some("12"), b"{}")
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 403"), "{error}");
    }

    #[test]
    fn garbage_body_is_an_error_not_a_panic() {
        assert!(parse_response(200, None, b"<html>").is_err());
        assert!(parse_response(200, None, b"{}").is_err());
    }

    #[test]
    fn repo_validation() {
        assert!(is_valid_repo("antoniofernandodj/rustploy"));
        assert!(is_valid_repo("a.b/c_d-e"));
        for bad in [
            "", "rustploy", "a/", "/b", "a/b/c", "a b/c", "a/b?x=1", "../x", "a/..", "./x", "a/b#c",
        ] {
            assert!(!is_valid_repo(bad), "{bad:?} deveria ser recusado");
        }
    }
}
