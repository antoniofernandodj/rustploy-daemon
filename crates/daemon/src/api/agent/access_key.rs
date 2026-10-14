//! Chave de acesso `rpk1_…`: o endereço do daemon e o token numa string só.
//!
//! Formato: `rpk1_` + base64url (sem padding) de `{"u":"<url>","t":"<token>"}`.
//! O prefixo diz o que a string é (e deixa scanners de segredo acharem uma
//! chave vazada); o JSON evita inventar escape para os `:` e `/` da URL.
//!
//! Isto **esconde** o endereço, não o criptografa: quem tem a chave tem o
//! token, que é o segredo de verdade. Ver docs/plano-chave-de-acesso-e-mcp.md.

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

/// Prefixo da versão 1 do formato.
pub const PREFIX: &str = "rpk1_";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessKey {
    /// Base do daemon, ex. `https://rustploy.exemplo.com:9797`.
    #[serde(rename = "u")]
    pub url: String,
    /// O `[api].token` do daemon.
    #[serde(rename = "t")]
    pub token: String,
}

/// Qual parte da chave não confere — cada uma tem uma correção diferente, e é
/// isso que deixa um agente acertar na tentativa seguinte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyFormatError {
    Empty,
    MissingPrefix,
    NotBase64,
    NotJson(String),
    MissingField(&'static str),
}

impl KeyFormatError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "empty_key",
            Self::MissingPrefix => "missing_prefix",
            Self::NotBase64 => "not_base64url",
            Self::NotJson(_) => "not_json",
            Self::MissingField(_) => "missing_field",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Empty => "nenhuma chave informada".into(),
            Self::MissingPrefix => format!(
                "a chave não começa com {PREFIX} — cole a chave de acesso inteira, \
                 não só o token"
            ),
            Self::NotBase64 => format!(
                "o trecho depois de {PREFIX} não é base64url válido — a chave foi \
                 cortada ou ganhou caracteres no caminho?"
            ),
            Self::NotJson(e) => {
                format!("o conteúdo decodificado não é o JSON esperado {{\"u\",\"t\"}}: {e}")
            }
            Self::MissingField(campo) => {
                format!("o campo \"{campo}\" está vazio ou ausente dentro da chave")
            }
        }
    }
}

impl AccessKey {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(raw: &str) -> Result<Self, KeyFormatError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(KeyFormatError::Empty);
        }
        let corpo = raw
            .strip_prefix(PREFIX)
            .ok_or(KeyFormatError::MissingPrefix)?;
        // Aceita com ou sem padding: quem copia de um terminal às vezes ganha
        // um `=` de um encoder que não é o nosso.
        let bytes = URL_SAFE_NO_PAD
            .decode(corpo.trim_end_matches('='))
            .or_else(|_| URL_SAFE.decode(corpo))
            .map_err(|_| KeyFormatError::NotBase64)?;
        let key: AccessKey =
            serde_json::from_slice(&bytes).map_err(|e| KeyFormatError::NotJson(e.to_string()))?;
        if key.url.trim().is_empty() {
            return Err(KeyFormatError::MissingField("u"));
        }
        if key.token.trim().is_empty() {
            return Err(KeyFormatError::MissingField("t"));
        }
        Ok(AccessKey {
            url: key.url.trim().trim_end_matches('/').to_owned(),
            token: key.token.trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exemplo() -> AccessKey {
        AccessKey {
            url: "https://rustploy.exemplo.com:9797".into(),
            token: "a3f9".into(),
        }
    }

    #[test]
    fn ida_e_volta() {
        let chave = exemplo().encode();
        assert!(chave.starts_with(PREFIX));
        assert!(
            !chave.contains("rustploy.exemplo.com"),
            "endereço não pode ficar explícito"
        );
        assert_eq!(AccessKey::decode(&chave).unwrap(), exemplo());
        assert_eq!(
            AccessKey::decode(&format!("  {chave}\n")).unwrap(),
            exemplo()
        );
    }

    #[test]
    fn cada_erro_diz_qual_parte_falhou() {
        assert_eq!(AccessKey::decode(""), Err(KeyFormatError::Empty));
        assert_eq!(
            AccessKey::decode("a3f9"),
            Err(KeyFormatError::MissingPrefix)
        );
        assert_eq!(
            AccessKey::decode("rpk1_!!!"),
            Err(KeyFormatError::NotBase64)
        );
        let nao_json = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode("oi"));
        assert!(matches!(
            AccessKey::decode(&nao_json),
            Err(KeyFormatError::NotJson(_))
        ));
        let sem_token = format!(
            "{PREFIX}{}",
            URL_SAFE_NO_PAD.encode(r#"{"u":"http://x:1","t":""}"#)
        );
        assert_eq!(
            AccessKey::decode(&sem_token),
            Err(KeyFormatError::MissingField("t"))
        );
    }

    #[test]
    fn aceita_padding() {
        let com_padding = format!(
            "{PREFIX}{}",
            URL_SAFE.encode(serde_json::to_vec(&exemplo()).unwrap())
        );
        assert_eq!(AccessKey::decode(&com_padding).unwrap(), exemplo());
    }
}
