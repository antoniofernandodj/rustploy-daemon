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
}
