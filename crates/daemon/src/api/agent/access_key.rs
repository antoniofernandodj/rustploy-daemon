//! Token de conexão: o endereço do daemon e o token da API numa string só.
//!
//! Formato: base64 (padrão, com padding) de `host:porta::token` — ex.
//! `base64("rustploy.exemplo.com:9797::a3f9")`. O mesmo que
//! `base64.b64encode(b"host:porta::token")` do Python.
//!
//! O esquema (`http`/`https`) **não** vai no token: quem conecta descobre
//! sozinho (a GUI tenta `https` e cai para `http`; a webui já está na origem; o
//! daemon sabe se termina TLS). Os decodificadores aceitam também o alfabeto
//! url-safe e a falta de padding.
//!
//! Isto **esconde** o endereço, não o criptografa: quem tem o token de conexão
//! tem o token da API, que é o segredo de verdade. Ver
//! docs/plano-chave-de-acesso-e-mcp.md.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// Separa o endereço do token dentro do token de conexão decodificado.
pub const SEPARATOR: &str = "::";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessKey {
    /// `host:porta`, sem esquema — ex. `rustploy.exemplo.com:9797`.
    pub address: String,
    /// O `[api].token` do daemon.
    pub token: String,
}

/// Qual parte do token de conexão não confere — cada uma tem uma correção
/// diferente, e é isso que deixa um agente acertar na tentativa seguinte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyFormatError {
    Empty,
    NotBase64,
    MissingSeparator,
    MissingField(&'static str),
}

impl KeyFormatError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "empty_key",
            Self::NotBase64 => "not_base64",
            Self::MissingSeparator => "missing_separator",
            Self::MissingField(_) => "missing_field",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Empty => "nenhum token de conexão informado".into(),
            Self::NotBase64 => "o token de conexão não é base64 válido — foi cortado ou \
                                ganhou caracteres no caminho?"
                .into(),
            Self::MissingSeparator => format!(
                "o token de conexão decodificado não tem o formato host:porta{SEPARATOR}token \
                 — cole o token de conexão inteiro, não só o token da API"
            ),
            Self::MissingField(parte) => {
                format!("o {parte} está vazio dentro do token de conexão")
            }
        }
    }
}

impl AccessKey {
    pub fn encode(&self) -> String {
        STANDARD.encode(format!("{}{SEPARATOR}{}", self.address, self.token))
    }

    pub fn decode(raw: &str) -> Result<Self, KeyFormatError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(KeyFormatError::Empty);
        }
        // Normaliza para url-safe sem padding: aceita os dois alfabetos, com
        // ou sem `=` no fim.
        let normalized = raw
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_");
        let bytes = URL_SAFE_NO_PAD
            .decode(&normalized)
            .map_err(|_| KeyFormatError::NotBase64)?;
        // Bytes que não são texto: quase sempre o token da API colado no lugar
        // do de conexão (todo token hex/base64url também "decodifica").
        let text = String::from_utf8(bytes).map_err(|_| KeyFormatError::MissingSeparator)?;

        let (address, token) = split_address(&text).ok_or(KeyFormatError::MissingSeparator)?;
        // Tolera um esquema posto à mão (`https://host:porta::token`): ele não
        // faz parte do formato, quem conecta decide.
        let address = address.trim();
        let address = address
            .split_once("://")
            .map_or(address, |(_, rest)| rest)
            .trim_end_matches('/');
        let token = token.trim();
        if address.is_empty() {
            return Err(KeyFormatError::MissingField("endereço"));
        }
        if token.is_empty() {
            return Err(KeyFormatError::MissingField("token"));
        }
        Ok(AccessKey {
            address: address.to_owned(),
            token: token.to_owned(),
        })
    }
}

/// Parte `<endereço>::<token>` no primeiro `::` que não seja do próprio
/// endereço — um IPv6 entre colchetes (`[::1]:9797`) tem os seus, então a
/// busca começa depois do `]`.
fn split_address(text: &str) -> Option<(&str, &str)> {
    let authority_start = text.find("://").map_or(0, |i| i + 3);
    let search_from = if text[authority_start..].starts_with('[') {
        authority_start + text[authority_start..].find(']')?
    } else {
        authority_start
    };
    let at = search_from + text[search_from..].find(SEPARATOR)?;
    Some((&text[..at], &text[at + SEPARATOR.len()..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(address: &str, token: &str) -> AccessKey {
        AccessKey {
            address: address.into(),
            token: token.into(),
        }
    }

    #[test]
    fn igual_ao_b64encode_do_python() {
        // base64.b64encode(b"rustploy.exemplo.com:9797::a3f9_x-y").decode()
        let chave = key("rustploy.exemplo.com:9797", "a3f9_x-y").encode();
        assert_eq!(chave, "cnVzdHBsb3kuZXhlbXBsby5jb206OTc5Nzo6YTNmOV94LXk=");
        assert_eq!(
            AccessKey::decode(&chave).unwrap(),
            key("rustploy.exemplo.com:9797", "a3f9_x-y")
        );
        assert_eq!(
            AccessKey::decode(&format!("  {chave}\n")).unwrap(),
            key("rustploy.exemplo.com:9797", "a3f9_x-y")
        );
    }

    #[test]
    fn aceita_url_safe_sem_padding_e_esquema_posto_a_mao() {
        let chave = URL_SAFE_NO_PAD.encode("https://a.b:9797::t~?");
        assert_eq!(AccessKey::decode(&chave).unwrap(), key("a.b:9797", "t~?"));
    }

    #[test]
    fn ipv6_e_token_com_dois_pontos_duplos() {
        let chave = key("[::1]:9797", "x::y").encode();
        assert_eq!(AccessKey::decode(&chave).unwrap(), key("[::1]:9797", "x::y"));
    }

    #[test]
    fn cada_erro_diz_qual_parte_falhou() {
        assert_eq!(AccessKey::decode(""), Err(KeyFormatError::Empty));
        assert_eq!(AccessKey::decode("!!!"), Err(KeyFormatError::NotBase64));
        assert_eq!(
            AccessKey::decode(&STANDARD.encode("so-o-token")),
            Err(KeyFormatError::MissingSeparator)
        );
        assert_eq!(
            AccessKey::decode(&STANDARD.encode("a.b:1::")),
            Err(KeyFormatError::MissingField("token"))
        );
        assert_eq!(
            AccessKey::decode(&STANDARD.encode("::t")),
            Err(KeyFormatError::MissingField("endereço"))
        );
    }
}
