//! Cliente HTTPS de saída mínimo: um GET, hyper cru sobre `tokio-rustls`.
//!
//! Sem `reqwest` (regra do daemon: HTTP é hyper puro, ver `ingress/proxy.rs::forward`).
//! As CAs vêm do `webpki-roots`, embutido no binário, então a verificação do
//! certificado não depende do que a VPS tem instalado em `/etc/ssl`.
//!
//! Não segue redirecionamentos: devolve o status e o `Location` e quem chama decide.

use anyhow::{Context, Result, anyhow, bail};
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Request, body::Bytes, body::Incoming};
use hyper_util::rt::TokioIo;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::net::TcpStream;
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, crypto::ring::default_provider, pki_types::ServerName},
};

/// Resposta já lida por inteiro (o corpo é limitado por `max_body_bytes`).
#[derive(Debug)]
pub struct HttpsResponse {
    pub status: u16,
    /// Cabeçalhos com o nome em minúsculas.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpsResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Partes de uma URL `https://host[:porta]/caminho?consulta`.
#[derive(Debug, PartialEq, Eq)]
struct UrlParts {
    host: String,
    port: u16,
    path_and_query: String,
}

fn split_https_url(url: &str) -> Result<UrlParts> {
    let uri: hyper::Uri = url
        .parse()
        .with_context(|| format!("URL inválida: {url}"))?;
    if uri.scheme_str() != Some("https") {
        bail!("só https é aceito: {url}");
    }
    let host = uri
        .host()
        .ok_or_else(|| anyhow!("URL sem host: {url}"))?
        .to_string();
    Ok(UrlParts {
        host,
        port: uri.port_u16().unwrap_or(443),
        path_and_query: uri
            .path_and_query()
            .map(|path| path.as_str().to_string())
            .unwrap_or_else(|| "/".into()),
    })
}

fn tls_connector() -> Result<TlsConnector> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .context("versões de TLS indisponíveis")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// `GET url` com os cabeçalhos dados. `timeout` vale para a operação inteira
/// (conexão, TLS e leitura); o corpo maior que `max_body_bytes` é recusado.
pub async fn get(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
    max_body_bytes: usize,
) -> Result<HttpsResponse> {
    tokio::time::timeout(timeout, get_inner(url, headers, max_body_bytes))
        .await
        .map_err(|_| anyhow!("tempo esgotado ao consultar {url}"))?
}

async fn get_inner(
    url: &str,
    headers: &[(&str, &str)],
    max_body_bytes: usize,
) -> Result<HttpsResponse> {
    let (status, response_headers, body) = open(url, headers).await?;
    let body = Limited::new(body, max_body_bytes)
        .collect()
        .await
        .map_err(|error| anyhow!("resposta grande demais ou interrompida: {error}"))?
        .to_bytes()
        .to_vec();
    Ok(HttpsResponse {
        status,
        headers: response_headers,
        body,
    })
}

/// Conecta, faz o `GET` e devolve status, cabeçalhos e o corpo **ainda por ler**.
async fn open(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, Vec<(String, String)>, Incoming)> {
    let parts = split_https_url(url)?;

    let tcp = TcpStream::connect((parts.host.as_str(), parts.port))
        .await
        .with_context(|| format!("sem conexão com {}:{}", parts.host, parts.port))?;
    let server_name = ServerName::try_from(parts.host.clone())
        .map_err(|_| anyhow!("nome de servidor inválido: {}", parts.host))?;
    let tls = tls_connector()?
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("falha no TLS com {}", parts.host))?;

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .context("falha no handshake HTTP")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut request = Request::builder()
        .method("GET")
        .uri(&parts.path_and_query)
        .header("host", &parts.host)
        .header("connection", "close");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(Empty::<Bytes>::new())
        .context("requisição inválida")?;

    let response = sender
        .send_request(request)
        .await
        .context("falha ao enviar a requisição")?;

    let status = response.status().as_u16();
    let response_headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    Ok((status, response_headers, response.into_body()))
}

const MAX_REDIRECTS: usize = 5;

/// `host` é um dos `allowed` ou um subdomínio de algum (`objects.githubusercontent.com`
/// casa com `githubusercontent.com`; `evilgithubusercontent.com` não).
fn host_allowed(host: &str, allowed: &[&str]) -> bool {
    allowed
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

/// Para onde seguir depois desta resposta: `None` se não é redirecionamento.
/// Só aceita destino absoluto em `https`.
fn redirect_target(status: u16, headers: &[(String, String)]) -> Result<Option<String>> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let location = headers
        .iter()
        .find(|(name, _)| name == "location")
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| anyhow!("redirecionamento (HTTP {status}) sem Location"))?;
    if !location.starts_with("https://") {
        bail!("redirecionamento para um destino que não é https: {location}");
    }
    Ok(Some(location.to_string()))
}

/// Baixa `url` para `destination`, seguindo até [`MAX_REDIRECTS`] redirecionamentos, **somente
/// para hosts de `allowed_hosts`** (um redirecionamento para fora da lista é recusado). Grava
/// em `<destination>.partial` e só renomeia no fim, então um download interrompido nunca deixa
/// um arquivo que pareça completo. `timeout` vale para o download inteiro.
pub async fn download(
    url: &str,
    headers: &[(&str, &str)],
    destination: &Path,
    max_bytes: u64,
    timeout: Duration,
    allowed_hosts: &[&str],
) -> Result<()> {
    let mut partial = destination.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);

    let result = tokio::time::timeout(
        timeout,
        download_inner(url, headers, &partial, max_bytes, allowed_hosts),
    )
    .await
    .map_err(|_| anyhow!("tempo esgotado ao baixar {url}"))
    .and_then(|result| result);

    match result {
        Ok(()) => tokio::fs::rename(&partial, destination)
            .await
            .with_context(|| format!("não consegui gravar {}", destination.display())),
        Err(error) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(error)
        }
    }
}

async fn download_inner(
    url: &str,
    headers: &[(&str, &str)],
    partial: &Path,
    max_bytes: u64,
    allowed_hosts: &[&str],
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let host = split_https_url(&current)?.host;
        if !host_allowed(&host, allowed_hosts) {
            bail!("download recusado: {host} não está na lista de origens permitidas");
        }
        let (status, response_headers, mut body) = open(&current, headers).await?;
        if let Some(next) = redirect_target(status, &response_headers)? {
            current = next;
            continue;
        }
        if status != 200 {
            bail!("o servidor respondeu HTTP {status} em {host}");
        }

        let mut file = tokio::fs::File::create(partial)
            .await
            .with_context(|| format!("não consegui criar {}", partial.display()))?;
        let mut written: u64 = 0;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| anyhow!("download interrompido: {error}"))?;
            if let Ok(data) = frame.into_data() {
                written += data.len() as u64;
                if written > max_bytes {
                    bail!("o arquivo passa do limite de {max_bytes} bytes");
                }
                file.write_all(&data)
                    .await
                    .context("falha ao gravar o download")?;
            }
        }
        file.flush().await.context("falha ao gravar o download")?;
        return Ok(());
    }
    bail!("redirecionamentos demais (mais de {MAX_REDIRECTS})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_plain_url() {
        assert_eq!(
            split_https_url("https://api.github.com/repos/a/b/releases/latest").unwrap(),
            UrlParts {
                host: "api.github.com".into(),
                port: 443,
                path_and_query: "/repos/a/b/releases/latest".into(),
            }
        );
    }

    #[test]
    fn keeps_port_and_query() {
        let parts = split_https_url("https://example.com:8443/x?y=1").unwrap();
        assert_eq!(parts.port, 8443);
        assert_eq!(parts.path_and_query, "/x?y=1");
    }

    #[test]
    fn bare_host_gets_root_path() {
        assert_eq!(
            split_https_url("https://example.com")
                .unwrap()
                .path_and_query,
            "/"
        );
    }

    #[test]
    fn refuses_plain_http_and_garbage() {
        assert!(split_https_url("http://example.com/").is_err());
        assert!(split_https_url("ftp://example.com/").is_err());
        assert!(split_https_url("não é url").is_err());
    }

    #[test]
    fn allowed_hosts_match_exactly_or_as_subdomains_only() {
        let allowed = ["github.com", "githubusercontent.com"];
        for host in [
            "github.com",
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
        ] {
            assert!(host_allowed(host, &allowed), "{host}");
        }
        for host in [
            "evilgithub.com",
            "github.com.evil.com",
            "githubusercontent.com.evil.org",
            "example.com",
            "",
        ] {
            assert!(!host_allowed(host, &allowed), "{host}");
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn redirects_are_followed_only_to_absolute_https() {
        let location = headers(&[("location", "https://objects.githubusercontent.com/x")]);
        for status in [301, 302, 303, 307, 308] {
            assert_eq!(
                redirect_target(status, &location).unwrap().as_deref(),
                Some("https://objects.githubusercontent.com/x")
            );
        }
        assert_eq!(redirect_target(200, &location).unwrap(), None);
        assert!(redirect_target(302, &headers(&[])).is_err());
        assert!(redirect_target(302, &headers(&[("location", "http://example.com/x")])).is_err());
        assert!(redirect_target(302, &headers(&[("location", "/relativo")])).is_err());
    }

    #[tokio::test]
    async fn download_refuses_a_host_outside_the_list_before_connecting() {
        let directory = std::env::temp_dir().join(format!("rp-dl-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let destination = directory.join("a.deb");
        let error = download(
            "https://example.invalid/a.deb",
            &[],
            &destination,
            1024,
            Duration::from_secs(5),
            &["github.com"],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("não está na lista"), "{error}");
        assert!(!destination.exists());
        assert!(!directory.join("a.deb.partial").exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn tls_connector_builds_with_embedded_roots() {
        assert!(tls_connector().is_ok());
    }

    /// Estado local sem rede nenhuma: porta fechada vira erro claro, não trava.
    #[tokio::test]
    async fn closed_port_is_a_clear_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = get(
            &format!("https://127.0.0.1:{port}/"),
            &[],
            Duration::from_secs(5),
            1024,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("sem conexão"), "{error}");
    }
}
