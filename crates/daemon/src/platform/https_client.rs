//! Cliente HTTPS de saída mínimo: um GET, hyper cru sobre `tokio-rustls`.
//!
//! Sem `reqwest` (regra do daemon: HTTP é hyper puro, ver `ingress/proxy.rs::forward`).
//! As CAs vêm do `webpki-roots`, embutido no binário, então a verificação do
//! certificado não depende do que a VPS tem instalado em `/etc/ssl`.
//!
//! Não segue redirecionamentos: devolve o status e o `Location` e quem chama decide.

use anyhow::{Context, Result, anyhow, bail};
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Request, body::Bytes};
use hyper_util::rt::TokioIo;
use std::{sync::Arc, time::Duration};
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
    let body = Limited::new(response.into_body(), max_body_bytes)
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
