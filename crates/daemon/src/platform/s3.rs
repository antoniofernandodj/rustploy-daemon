//! Cliente S3 mínimo, em hyper cru (o daemon não usa `reqwest`), para o backup dos dados dos serviços
//! (`docs/plano-backup-de-dados-para-s3.md`, Fase 0).
//!
//! Fala com qualquer S3 (AWS, Backblaze, R2, MinIO/RustFS): assinatura SigV4, `http` ou `https`,
//! endereço `path-style` (`host/bucket/chave`) ou `virtual-host` (`bucket.host/chave`).
//!
//! O corpo de cada requisição vai **em memória com o SHA-256 real** (nada de `UNSIGNED-PAYLOAD`: um
//! endpoint `http` local o recusa). Por isso um objeto grande sobe em **partes** (multipart) de
//! [`PART_SIZE`]: a memória nunca passa de uma parte. O download vai direto para um arquivo.

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, body::Bytes, body::Incoming};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;

type HmacSha256 = Hmac<Sha256>;

/// Tamanho de cada parte do multipart (o S3 exige no mínimo 5 MiB, exceto na última).
pub const PART_SIZE: usize = 32 * 1024 * 1024;
/// Até este tamanho o objeto sobe num único PUT.
const SINGLE_PUT_MAX: u64 = PART_SIZE as u64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_XML: usize = 8 * 1024 * 1024;
const ATTEMPTS: u32 = 3;

/// Destino S3. `secret_key` é o valor **já decifrado** (quem chama resolve o secret).
#[derive(Clone)]
pub struct S3Config {
    /// `https://s3.us-east-005.backblazeb2.com`, `http://rp_s3:9000`… (sem o bucket).
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// `true`: `host/bucket/chave` (MinIO, RustFS, R2…). `false`: `bucket.host/chave` (AWS).
    pub path_style: bool,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key", &self.access_key)
            .field("secret_key", &"***")
            .field("path_style", &self.path_style)
            .finish()
    }
}

/// Um objeto da listagem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub last_modified: String,
}

impl S3Config {
    pub fn validate(&self) -> Result<()> {
        let uri: hyper::Uri = self
            .endpoint
            .parse()
            .with_context(|| format!("endpoint inválido: {}", self.endpoint))?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
            bail!("o endpoint precisa começar com http:// ou https://");
        }
        if self.bucket.trim().is_empty() {
            bail!("informe o bucket");
        }
        if self.access_key.trim().is_empty() || self.secret_key.is_empty() {
            bail!("informe a chave de acesso e a chave secreta");
        }
        if self.region.trim().is_empty() {
            bail!("informe a região (use `us-east-1` se o provedor não pedir uma)");
        }
        Ok(())
    }
}

// ── assinatura SigV4 ────────────────────────────────────────────────────────

/// Codifica como o SigV4 pede (só `A-Z a-z 0-9 - _ . ~` ficam; `/` fica se `keep_slash`).
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC aceita chave de qualquer tamanho");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Query canônica: pares codificados e ordenados.
fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// O cabeçalho `Authorization` de uma requisição. Assina `host`, `x-amz-content-sha256` e `x-amz-date`.
/// `path` já vem codificado (é o que vai na linha de requisição).
#[allow(clippy::too_many_arguments)]
fn authorization(
    method: &str,
    host: &str,
    path: &str,
    query: &[(String, String)],
    payload_hash: &str,
    amz_date: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> String {
    let date = &amz_date[..8];
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical = format!(
        "{method}\n{path}\n{}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n\n{signed_headers}\n{payload_hash}",
        canonical_query(query)
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

// ── transporte ──────────────────────────────────────────────────────────────

struct Target {
    /// Cabeçalho `Host` (com a porta quando não é a padrão).
    host_header: String,
    /// Onde conectar.
    connect_host: String,
    port: u16,
    tls: bool,
    /// Caminho já codificado.
    path: String,
}

fn target(cfg: &S3Config, key: &str) -> Result<Target> {
    let uri: hyper::Uri = cfg.endpoint.parse().context("endpoint inválido")?;
    let tls = uri.scheme_str() == Some("https");
    let base_host = uri.host().ok_or_else(|| anyhow!("endpoint sem host"))?;
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let default_port = if tls { 443 } else { 80 };
    let (connect_host, path) = if cfg.path_style {
        (
            base_host.to_string(),
            format!(
                "/{}/{}",
                uri_encode(&cfg.bucket, false),
                uri_encode(key, true)
            ),
        )
    } else {
        (
            format!("{}.{base_host}", cfg.bucket),
            format!("/{}", uri_encode(key, true)),
        )
    };
    let host_header = if port == default_port {
        connect_host.clone()
    } else {
        format!("{connect_host}:{port}")
    };
    Ok(Target {
        host_header,
        connect_host,
        port,
        tls,
        path,
    })
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Incoming,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    async fn text(self) -> Result<String> {
        let bytes = Limited::new(self.body, MAX_XML)
            .collect()
            .await
            .map_err(|e| anyhow!("resposta grande demais ou interrompida: {e}"))?
            .to_bytes();
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

async fn send_once(
    cfg: &S3Config,
    method: &str,
    key: &str,
    query: &[(String, String)],
    body: &Bytes,
) -> Result<Reply> {
    let t = target(cfg, key)?;
    let amz_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let payload_hash = sha256_hex(body);
    let auth = authorization(
        method,
        &t.host_header,
        &t.path,
        query,
        &payload_hash,
        &amz_date,
        &cfg.region,
        &cfg.access_key,
        &cfg.secret_key,
    );
    let q = canonical_query(query);
    let uri = if q.is_empty() {
        t.path.clone()
    } else {
        format!("{}?{q}", t.path)
    };
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", &t.host_header)
        .header("x-amz-content-sha256", &payload_hash)
        .header("x-amz-date", &amz_date)
        .header("authorization", auth)
        .header("connection", "close")
        .body(Full::new(body.clone()))
        .context("requisição inválida")?;

    let tcp = TcpStream::connect((t.connect_host.as_str(), t.port))
        .await
        .with_context(|| format!("sem conexão com {}:{}", t.connect_host, t.port))?;
    let response = if t.tls {
        let name = ServerName::try_from(t.connect_host.clone())
            .map_err(|_| anyhow!("nome de servidor inválido: {}", t.connect_host))?;
        let tls = super::https_client::tls_connector()?
            .connect(name, tcp)
            .await
            .with_context(|| format!("falha no TLS com {}", t.connect_host))?;
        exchange(TokioIo::new(tls), request).await?
    } else {
        exchange(TokioIo::new(tcp), request).await?
    };
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(n, v)| {
            v.to_str()
                .ok()
                .map(|v| (n.as_str().to_string(), v.to_string()))
        })
        .collect();
    Ok(Reply {
        status,
        headers,
        body: response.into_body(),
    })
}

async fn exchange<I>(io: I, request: Request<Full<Bytes>>) -> Result<hyper::Response<Incoming>>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .context("falha no handshake HTTP")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
        .send_request(request)
        .await
        .context("falha ao enviar a requisição")
}

/// Envia com até [`ATTEMPTS`] tentativas em falha de rede ou 5xx (espera 1 s, 2 s).
async fn send(
    cfg: &S3Config,
    method: &str,
    key: &str,
    query: &[(&str, &str)],
    body: Bytes,
) -> Result<Reply> {
    let query: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut last = anyhow!("sem tentativas");
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, send_once(cfg, method, key, &query, &body))
            .await
        {
            Err(_) => last = anyhow!("tempo esgotado em {method} {key}"),
            Ok(Err(e)) => last = e,
            Ok(Ok(reply)) if reply.status >= 500 => {
                let status = reply.status;
                let text = reply.text().await.unwrap_or_default();
                last = anyhow!("o S3 respondeu {status}: {}", s3_error(&text));
            }
            Ok(Ok(reply)) => return Ok(reply),
        }
    }
    Err(last)
}

/// `Code: Message` de um corpo de erro do S3.
fn s3_error(xml: &str) -> String {
    match (first_tag(xml, "Code"), first_tag(xml, "Message")) {
        (Some(c), Some(m)) => format!("{c}: {m}"),
        (Some(c), None) => c,
        _ => xml.chars().take(200).collect(),
    }
}

/// Confere o status e, se falhou, devolve o erro com a causa do S3.
async fn expect_ok(reply: Reply, what: &str) -> Result<Reply> {
    if (200..300).contains(&reply.status) {
        return Ok(reply);
    }
    let status = reply.status;
    let text = reply.text().await.unwrap_or_default();
    Err(anyhow!(
        "{what}: o S3 respondeu {status} ({})",
        s3_error(&text)
    ))
}

// ── XML (só o que as respostas do S3 usam) ──────────────────────────────────

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Conteúdo da primeira `<tag>…</tag>`.
fn first_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(&xml[start..end]))
}

/// Os blocos `<tag>…</tag>` (conteúdo cru), na ordem.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(s) = rest.find(&open) {
        let after = &rest[s + open.len()..];
        let Some(e) = after.find(&close) else { break };
        out.push(&after[..e]);
        rest = &after[e + close.len()..];
    }
    out
}

// ── operações ───────────────────────────────────────────────────────────────

/// Lê até `buf.len()` bytes (menos só no fim do arquivo). Devolve quantos leu.
async fn read_full(f: &mut tokio::fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let r = f.read(&mut buf[n..]).await?;
        if r == 0 {
            break;
        }
        n += r;
    }
    Ok(n)
}

/// Sobe `path` como o objeto `key`. Um PUT até [`SINGLE_PUT_MAX`]; acima disso, multipart (e
/// aborta o multipart se algo falhar, para não deixar partes órfãs cobrando espaço).
pub async fn put_file(cfg: &S3Config, key: &str, path: &Path) -> Result<()> {
    let size = tokio::fs::metadata(path).await?.len();
    let mut file = tokio::fs::File::open(path).await?;
    if size <= SINGLE_PUT_MAX {
        let mut buf = vec![0u8; size as usize];
        let n = read_full(&mut file, &mut buf).await?;
        buf.truncate(n);
        let reply = send(cfg, "PUT", key, &[], Bytes::from(buf)).await?;
        expect_ok(reply, "enviar o objeto").await?;
        return Ok(());
    }

    let reply = send(cfg, "POST", key, &[("uploads", "")], Bytes::new()).await?;
    let text = expect_ok(reply, "iniciar o envio em partes")
        .await?
        .text()
        .await?;
    let upload_id =
        first_tag(&text, "UploadId").ok_or_else(|| anyhow!("o S3 não devolveu o UploadId"))?;

    let result = upload_parts(cfg, key, &upload_id, &mut file).await;
    if result.is_err() {
        let _ = send(
            cfg,
            "DELETE",
            key,
            &[("uploadId", &upload_id)],
            Bytes::new(),
        )
        .await;
    }
    result
}

async fn upload_parts(
    cfg: &S3Config,
    key: &str,
    upload_id: &str,
    file: &mut tokio::fs::File,
) -> Result<()> {
    let mut etags: Vec<(u32, String)> = Vec::new();
    let mut buf = vec![0u8; PART_SIZE];
    let mut number = 1u32;
    loop {
        let n = read_full(file, &mut buf).await?;
        if n == 0 {
            break;
        }
        let part = number.to_string();
        let reply = send(
            cfg,
            "PUT",
            key,
            &[("partNumber", &part), ("uploadId", upload_id)],
            Bytes::copy_from_slice(&buf[..n]),
        )
        .await?;
        let reply = expect_ok(reply, &format!("enviar a parte {number}")).await?;
        let etag = reply
            .header("etag")
            .ok_or_else(|| anyhow!("o S3 não devolveu o ETag da parte {number}"))?
            .to_string();
        etags.push((number, etag));
        number += 1;
        if n < PART_SIZE {
            break;
        }
    }
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (n, etag) in &etags {
        xml.push_str(&format!(
            "<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag></Part>",
            xml_escape(etag)
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");
    let reply = send(
        cfg,
        "POST",
        key,
        &[("uploadId", upload_id)],
        Bytes::from(xml),
    )
    .await?;
    let text = expect_ok(reply, "concluir o envio em partes")
        .await?
        .text()
        .await?;
    // o S3 pode responder 200 com um <Error> no corpo
    if text.contains("<Error>") {
        bail!("concluir o envio em partes: {}", s3_error(&text));
    }
    Ok(())
}

/// Baixa `key` para `dest`. Grava em `<dest>.partial` e só renomeia no fim: um download interrompido
/// nunca deixa um arquivo que pareça completo.
pub async fn get_to_file(cfg: &S3Config, key: &str, dest: &Path) -> Result<u64> {
    let reply = send(cfg, "GET", key, &[], Bytes::new()).await?;
    let mut reply = expect_ok(reply, "baixar o objeto").await?;
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);
    let result: Result<u64> = async {
        let mut out = tokio::fs::File::create(&partial).await?;
        let mut total = 0u64;
        while let Some(frame) = reply.body.frame().await {
            let frame = frame.context("download interrompido")?;
            if let Some(chunk) = frame.data_ref() {
                out.write_all(chunk).await?;
                total += chunk.len() as u64;
            }
        }
        out.flush().await?;
        Ok(total)
    }
    .await;
    match result {
        Ok(total) => {
            tokio::fs::rename(&partial, dest).await?;
            Ok(total)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(e)
        }
    }
}

/// Lista os objetos sob `prefix` (segue a paginação).
pub async fn list(cfg: &S3Config, prefix: &str) -> Result<Vec<Object>> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut query = vec![("list-type", "2"), ("prefix", prefix)];
        if let Some(t) = token.as_deref() {
            query.push(("continuation-token", t));
        }
        let reply = send(cfg, "GET", "", &query, Bytes::new()).await?;
        let text = expect_ok(reply, "listar os objetos").await?.text().await?;
        for b in blocks(&text, "Contents") {
            let (Some(key), Some(size)) = (first_tag(b, "Key"), first_tag(b, "Size")) else {
                continue;
            };
            out.push(Object {
                key,
                size: size.parse().unwrap_or(0),
                last_modified: first_tag(b, "LastModified").unwrap_or_default(),
            });
        }
        if first_tag(&text, "IsTruncated").as_deref() == Some("true") {
            token = first_tag(&text, "NextContinuationToken");
            if token.is_none() {
                break;
            }
        } else {
            break;
        }
    }
    Ok(out)
}

/// Apaga `key` (apagar o que não existe também é sucesso, como no S3).
pub async fn delete(cfg: &S3Config, key: &str) -> Result<()> {
    let reply = send(cfg, "DELETE", key, &[], Bytes::new()).await?;
    expect_ok(reply, "apagar o objeto").await?;
    Ok(())
}

/// **Testar conexão**: grava um objeto pequeno, lê de volta, confere e apaga. Cobre credencial,
/// bucket, região, endereço (path/virtual-host) e as três permissões de que o backup precisa.
pub async fn test(cfg: &S3Config, prefix: &str) -> Result<()> {
    cfg.validate()?;
    let key = format!(
        "{}.rustploy-teste-{}",
        prefix_with_slash(prefix),
        Utc::now().format("%Y%m%d%H%M%S")
    );
    // dois arquivos temporários com nome único (o `tempfile` é só dependência de teste)
    let stamp = format!("{}-{}", std::process::id(), Utc::now().timestamp_micros());
    let src = std::env::temp_dir().join(format!("rustploy-s3-origem-{stamp}"));
    let dst = std::env::temp_dir().join(format!("rustploy-s3-destino-{stamp}"));
    let body = b"rustploy: teste de conexao com o S3";
    let result = async {
        tokio::fs::write(&src, body).await?;
        put_file(cfg, &key, &src).await.context("gravar")?;
        let got = get_to_file(cfg, &key, &dst).await.context("ler");
        let _ = delete(cfg, &key).await;
        got?;
        if tokio::fs::read(&dst).await? != body {
            bail!("o objeto lido não é igual ao gravado");
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&src).await;
    let _ = tokio::fs::remove_file(&dst).await;
    result
}

fn prefix_with_slash(prefix: &str) -> String {
    let p = prefix.trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vetor oficial da AWS: "GET Bucket (List Objects)" da documentação do SigV4 para S3.
    #[test]
    fn assinatura_confere_com_o_vetor_da_aws() {
        let auth = authorization(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/",
            &[
                ("max-keys".into(), "2".into()),
                ("prefix".into(), "J".into()),
            ],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "20130524T000000Z",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert!(
            auth.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{auth}"
        );
        assert!(
            auth.contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request")
        );
    }

    #[test]
    fn codifica_a_chave_e_preserva_a_barra() {
        assert_eq!(uri_encode("a b/ç~.txt", true), "a%20b/%C3%A7~.txt");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn query_canonica_ordena_e_codifica() {
        let q = canonical_query(&[
            ("prefix".into(), "a b".into()),
            ("list-type".into(), "2".into()),
            ("uploads".into(), "".into()),
        ]);
        assert_eq!(q, "list-type=2&prefix=a%20b&uploads=");
    }

    fn cfg(path_style: bool, endpoint: &str) -> S3Config {
        S3Config {
            endpoint: endpoint.into(),
            region: "us-east-1".into(),
            bucket: "bk".into(),
            access_key: "ak".into(),
            secret_key: "sk".into(),
            path_style,
        }
    }

    #[test]
    fn endereco_path_style_e_virtual_host() {
        let t = target(&cfg(true, "http://rp_s3:9000"), "p/a b").unwrap();
        assert_eq!(
            (t.host_header.as_str(), t.path.as_str(), t.tls, t.port),
            ("rp_s3:9000", "/bk/p/a%20b", false, 9000)
        );
        let t = target(&cfg(false, "https://s3.amazonaws.com"), "p/x").unwrap();
        assert_eq!(
            (
                t.host_header.as_str(),
                t.connect_host.as_str(),
                t.path.as_str(),
                t.tls
            ),
            ("bk.s3.amazonaws.com", "bk.s3.amazonaws.com", "/p/x", true)
        );
    }

    #[test]
    fn valida_a_configuracao() {
        assert!(cfg(true, "https://x").validate().is_ok());
        assert!(cfg(true, "ftp://x").validate().is_err());
        let mut c = cfg(true, "https://x");
        c.bucket = " ".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn lista_le_os_objetos_e_o_erro() {
        let xml = "<ListBucketResult><IsTruncated>false</IsTruncated>\
            <Contents><Key>a/1.age</Key><LastModified>2026-11-11T10:00:00Z</LastModified><Size>10</Size></Contents>\
            <Contents><Key>a/2&amp;.age</Key><LastModified>2026-11-12T10:00:00Z</LastModified><Size>20</Size></Contents>\
            </ListBucketResult>";
        let b = blocks(xml, "Contents");
        assert_eq!(b.len(), 2);
        assert_eq!(first_tag(b[1], "Key").unwrap(), "a/2&.age");
        assert_eq!(first_tag(b[1], "Size").unwrap(), "20");
        assert_eq!(
            s3_error("<Error><Code>NoSuchBucket</Code><Message>x</Message></Error>"),
            "NoSuchBucket: x"
        );
    }

    /// Contra um S3 de verdade: `RP_TEST_S3=http://127.0.0.1:19000 RP_TEST_S3_BUCKET=rp-backup
    /// RP_TEST_S3_AK=… RP_TEST_S3_SK=… cargo test -p rustploy --bin rustployd s3_de_verdade -- --ignored`
    /// (RustFS/MinIO em Podman; o bucket já tem de existir).
    #[tokio::test]
    #[ignore]
    async fn s3_de_verdade_ida_e_volta_com_multipart() {
        let var = |n: &str| std::env::var(n).unwrap_or_else(|_| panic!("falta {n}"));
        let c = S3Config {
            endpoint: var("RP_TEST_S3"),
            region: "us-east-1".into(),
            bucket: var("RP_TEST_S3_BUCKET"),
            access_key: var("RP_TEST_S3_AK"),
            secret_key: var("RP_TEST_S3_SK"),
            path_style: true,
        };
        test(&c, "rustploy-teste").await.expect("testar conexão");

        let dir = tempfile::tempdir().unwrap();
        // 70 MiB pseudoaleatórios: 3 partes (32 + 32 + 6)
        let mut big = vec![0u8; 70 * 1024 * 1024];
        let mut x = 0x9E37_79B9u32;
        for b in big.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        let src = dir.path().join("grande");
        std::fs::write(&src, &big).unwrap();
        let small = dir.path().join("pequeno");
        std::fs::write(&small, b"oi").unwrap();

        put_file(&c, "rustploy-teste/a b/grande.bin", &src)
            .await
            .expect("multipart");
        put_file(&c, "rustploy-teste/a b/pequeno.bin", &small)
            .await
            .expect("put simples");

        let listed = list(&c, "rustploy-teste/a b/").await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        let g = listed
            .iter()
            .find(|o| o.key.ends_with("grande.bin"))
            .unwrap();
        assert_eq!(g.size, big.len() as u64);

        let dst = dir.path().join("baixado");
        let n = get_to_file(&c, "rustploy-teste/a b/grande.bin", &dst)
            .await
            .unwrap();
        assert_eq!(n, big.len() as u64);
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            big,
            "o conteúdo mudou na ida e volta"
        );

        for o in &listed {
            delete(&c, &o.key).await.unwrap();
        }
        assert!(list(&c, "rustploy-teste/a b/").await.unwrap().is_empty());
        assert!(
            get_to_file(&c, "rustploy-teste/nao-existe", &dir.path().join("x"))
                .await
                .is_err()
        );
    }

    #[test]
    fn debug_nao_vaza_a_chave_secreta() {
        assert!(!format!("{:?}", cfg(true, "https://x")).contains("sk\""));
    }
}
