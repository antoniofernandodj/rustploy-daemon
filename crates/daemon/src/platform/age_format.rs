//! O formato `ENC[age:<base64>]` dos segredos, **sem nenhuma dependência do daemon** (nada de banco,
//! config ou log): é compartilhado com o `rustploy-cli` (`src/bin/rustploy-cli.rs`, via `#[path]`),
//! para que o que o CLI cifra seja exatamente o que o daemon decifra. Plano:
//! `docs/plano-backup-estado-em-git.md`, R5/R6.

use age::{Decryptor, Recipient, x25519};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use std::io::{Read, Write};

pub const ENC_PREFIX: &str = "ENC[age:";

/// O valor está no formato `ENC[age:…]`?
pub fn is_enc(value: &str) -> bool {
    value.starts_with(ENC_PREFIX) && value.ends_with(']')
}

/// Cifra para todos os `recipients` (a chave do servidor e, se houver, a de recuperação).
pub fn encrypt(plaintext: &str, recipients: &[x25519::Recipient]) -> Result<String> {
    let boxed: Vec<Box<dyn Recipient + Send>> = recipients
        .iter()
        .cloned()
        .map(|r| Box::new(r) as Box<dyn Recipient + Send>)
        .collect();
    let encryptor = age::Encryptor::with_recipients(boxed)
        .ok_or_else(|| anyhow!("nenhum destinatário para cifrar"))?;
    let mut encrypted = Vec::new();
    {
        let mut writer = encryptor.wrap_output(&mut encrypted)?;
        writer.write_all(plaintext.as_bytes())?;
        writer.finish()?;
    }
    Ok(format!("{ENC_PREFIX}{}]", B64.encode(&encrypted)))
}

/// Decifra um `ENC[age:…]` com a identidade.
pub fn decrypt(value: &str, identity: &x25519::Identity) -> Result<String> {
    if !is_enc(value) {
        bail!("o valor não está no formato ENC[age:…]");
    }
    let inner = &value[ENC_PREFIX.len()..value.len() - 1];
    let bytes = B64
        .decode(inner)
        .context("ENC[age:…] com base64 inválido")?;
    let decryptor = match Decryptor::new(&bytes[..])? {
        Decryptor::Recipients(d) => d,
        _ => bail!("segredo cifrado com passphrase onde se esperava uma identidade"),
    };
    let mut out = Vec::new();
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| anyhow!("não consegui decifrar com esta identidade: {e}"))?;
    reader.read_to_end(&mut out)?;
    Ok(String::from_utf8(out)?)
}

/// Cabeçalho de todo arquivo age (binário).
#[allow(dead_code)] // o rustploy-cli inclui este arquivo por #[path] e não cifra arquivos
const AGE_MAGIC: &[u8] = b"age-encryption.org/v1";

/// O arquivo `path` é um arquivo age? (Só olha os primeiros bytes.)
#[allow(dead_code)] // idem: só o daemon usa
pub fn is_age_file(path: &std::path::Path) -> bool {
    let mut head = vec![0u8; AGE_MAGIC.len()];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && head == AGE_MAGIC
}

/// Cifra o arquivo `src` em `dst` (em fluxo: não carrega o arquivo na memória).
#[allow(dead_code)] // idem: só o daemon usa
pub fn encrypt_file(
    src: &std::path::Path,
    dst: &std::path::Path,
    recipients: &[x25519::Recipient],
) -> Result<()> {
    let boxed: Vec<Box<dyn Recipient + Send>> = recipients
        .iter()
        .cloned()
        .map(|r| Box::new(r) as Box<dyn Recipient + Send>)
        .collect();
    let encryptor = age::Encryptor::with_recipients(boxed)
        .ok_or_else(|| anyhow!("nenhum destinatário para cifrar"))?;
    let mut input = std::fs::File::open(src)?;
    let mut writer = encryptor.wrap_output(std::fs::File::create(dst)?)?;
    std::io::copy(&mut input, &mut writer)?;
    writer.finish()?;
    Ok(())
}

/// Decifra o arquivo age `src` em `dst` com a identidade (em fluxo).
#[allow(dead_code)] // idem: só o daemon usa
pub fn decrypt_file(
    src: &std::path::Path,
    dst: &std::path::Path,
    identity: &x25519::Identity,
) -> Result<()> {
    let decryptor = match Decryptor::new(std::fs::File::open(src)?)? {
        Decryptor::Recipients(d) => d,
        _ => bail!("arquivo cifrado com passphrase onde se esperava uma identidade"),
    };
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| anyhow!("não consegui decifrar com a identidade deste servidor: {e}"))?;
    let mut out = std::fs::File::create(dst)?;
    std::io::copy(&mut reader, &mut out)?;
    Ok(())
}

/// A `identity` abre o arquivo age cujo **início** é `head`? Só lê o cabeçalho (onde ficam as chaves de
/// arquivo cifradas por destinatário), nunca o conteúdo: bastam os primeiros KB do arquivo, sem baixar um
/// backup inteiro para saber se a chave serve.
///
/// `Ok(false)` = o arquivo é válido mas **não foi cifrado para esta chave**. `Err` = não deu para ler o
/// cabeçalho (cortado curto demais, ou não é um arquivo age): aí não dá para afirmar nada.
#[allow(dead_code)] // idem: só o daemon usa
pub fn head_opens(head: &[u8], identity: &x25519::Identity) -> Result<bool> {
    let decryptor = match Decryptor::new(head)? {
        Decryptor::Recipients(d) => d,
        _ => bail!("arquivo cifrado com passphrase onde se esperava uma identidade"),
    };
    match decryptor.decrypt(std::iter::once(identity as &dyn age::Identity)) {
        Ok(_) => Ok(true),
        Err(age::DecryptError::NoMatchingKeys) => Ok(false),
        Err(e) => Err(anyhow!("não consegui verificar o cabeçalho: {e}")),
    }
}

pub fn parse_identity(text: &str) -> Result<x25519::Identity> {
    text.trim()
        .parse::<x25519::Identity>()
        .map_err(|e| anyhow!("{e}"))
}

pub fn parse_recipient(text: &str) -> Result<x25519::Recipient> {
    text.trim()
        .parse::<x25519::Recipient>()
        .map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_e_volta_com_varios_destinatarios() {
        let (a, b) = (x25519::Identity::generate(), x25519::Identity::generate());
        let c = encrypt("segredo", &[a.to_public(), b.to_public()]).unwrap();
        assert!(is_enc(&c) && !c.contains("segredo"));
        assert_eq!(decrypt(&c, &a).unwrap(), "segredo");
        assert_eq!(decrypt(&c, &b).unwrap(), "segredo");
        assert!(decrypt(&c, &x25519::Identity::generate()).is_err());
    }

    #[test]
    fn arquivo_ida_e_volta_so_com_a_identidade_certa() {
        let d = tempfile::tempdir().unwrap();
        let (src, enc, dec) = (
            d.path().join("a"),
            d.path().join("a.age"),
            d.path().join("b"),
        );
        std::fs::write(&src, vec![7u8; 300_000]).unwrap();
        let (a, b) = (x25519::Identity::generate(), x25519::Identity::generate());
        encrypt_file(&src, &enc, &[a.to_public()]).unwrap();
        assert!(is_age_file(&enc) && !is_age_file(&src));
        assert!(decrypt_file(&enc, &dec, &b).is_err());
        decrypt_file(&enc, &dec, &a).unwrap();
        assert_eq!(std::fs::read(&dec).unwrap(), std::fs::read(&src).unwrap());
    }

    #[test]
    fn sem_destinatario_e_valor_fora_do_formato_dao_erro() {
        assert!(encrypt("x", &[]).is_err());
        let id = x25519::Identity::generate();
        assert!(decrypt("texto-puro", &id).is_err());
        assert!(decrypt("ENC[age:!!!]", &id).is_err());
    }

    #[test]
    fn parse_de_chaves() {
        let id = x25519::Identity::generate();
        use age::secrecy::ExposeSecret;
        let sk = id.to_string();
        assert!(parse_identity(sk.expose_secret()).is_ok());
        assert!(parse_recipient(&id.to_public().to_string()).is_ok());
        assert!(parse_identity("age1naoeprivada").is_err());
        assert!(parse_recipient("AGE-SECRET-KEY-1X").is_err());
    }

    /// O caso real: backup cifrado para a chave A; a pessoa só tem a B (a chave de recuperação foi trocada).
    /// Tem de dar para saber isso lendo só o início do arquivo.
    #[test]
    fn head_opens_distingue_a_chave_certa_da_errada_so_pelo_cabecalho() {
        let (certa, errada) = (x25519::Identity::generate(), x25519::Identity::generate());
        let boxed: Vec<Box<dyn Recipient + Send>> = vec![Box::new(certa.to_public())];
        let mut cifrado = Vec::new();
        let mut w = age::Encryptor::with_recipients(boxed)
            .unwrap()
            .wrap_output(&mut cifrado)
            .unwrap();
        w.write_all(&vec![7u8; 100_000]).unwrap();
        w.finish().unwrap();

        let inicio = &cifrado[..2048]; // 2 KB de um arquivo de ~100 KB
        assert!(head_opens(inicio, &certa).unwrap());
        assert!(!head_opens(inicio, &errada).unwrap());
        // cabeçalho cortado no meio: não dá para afirmar nada (erro, não "não abre")
        assert!(head_opens(&cifrado[..10], &certa).is_err());
        // não é um arquivo age
        assert!(head_opens(b"isto nao e age", &certa).is_err());
    }
}
