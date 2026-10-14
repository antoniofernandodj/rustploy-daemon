//! Chave de acesso: o endereço do daemon e o token numa string só.
//!
//! Formato: base64url (sem padding) de `<endereço>::<token>`, onde o endereço
//! é `host:porta` — ex. `base64("rustploy.exemplo.com:9797::a3f9")`.
//!
//! O esquema não vai na chave quando dá para deduzi-lo: porta 443 (ou sem
//! porta) é `https`, qualquer outra é `http`. Quando a dedução erraria (`https`
//! numa porta que não é a 443, `http` na 443), a chave leva o esquema por
//! extenso: `https://host:9797::token`.
//!
//! Isto **esconde** o endereço, não o criptografa: quem tem a chave tem o
//! token, que é o segredo de verdade. Ver docs/plano-chave-de-acesso-e-mcp.md.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// Separa o endereço do token dentro da chave decodificada.
pub const SEPARATOR: &str = "::";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessKey {
    /// Base do daemon, ex. `https://rustploy.exemplo.com:9797`.
    pub url: String,
    /// O `[api].token` do daemon.
    pub token: String,
}

/// Qual parte da chave não confere — cada uma tem uma correção diferente, e é
/// isso que deixa um agente acertar na tentativa seguinte.
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
            Self::Empty => "nenhuma chave informada".into(),
            Self::NotBase64 => "a chave não é base64 válido — foi cortada ou ganhou \
                                caracteres no caminho?"
                .into(),
            Self::MissingSeparator => format!(
                "a chave decodificada não tem o formato host:porta{SEPARATOR}token — \
                 cole a chave de acesso inteira, não só o token"
            ),
            Self::MissingField(parte) => {
                format!("o {parte} está vazio dentro da chave")
            }
        }
    }
}

/// Esquema deduzido de um endereço sem esquema: só a porta 443 (ou nenhuma
/// porta) é `https`.
fn implied_scheme(authority: &str) -> &'static str {
    // `[::1]:9797` — a porta é o que vem depois do `]`.
    let after_host = authority.rsplit_once(']').map_or(authority, |(_, rest)| rest);
    match after_host.rsplit_once(':') {
        Some((_, port)) if port != "443" => "http",
        _ => "https",
    }
}

impl AccessKey {
    pub fn encode(&self) -> String {
        let url = self.url.trim_end_matches('/');
        let address = match url.split_once("://") {
            Some((scheme, authority)) if implied_scheme(authority) == scheme => authority,
            _ => url,
        };
        URL_SAFE_NO_PAD.encode(format!("{address}{SEPARATOR}{}", self.token))
    }

    pub fn decode(raw: &str) -> Result<Self, KeyFormatError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(KeyFormatError::Empty);
        }
        // Aceita o alfabeto padrão e o padding também: a chave às vezes passa
        // por um encoder que não é o nosso (`echo -n … | base64`).
        let normalized = raw.trim_end_matches('=').replace('+', "-").replace('/', "_");
        let bytes = URL_SAFE_NO_PAD
            .decode(&normalized)
            .or_else(|_| STANDARD.decode(raw))
            .map_err(|_| KeyFormatError::NotBase64)?;
        // Bytes que não são texto: quase sempre um token cru colado no lugar
        // da chave (todo token hex/base64url também "decodifica").
        let text = String::from_utf8(bytes).map_err(|_| KeyFormatError::MissingSeparator)?;

        let (address, token) = split_address(&text).ok_or(KeyFormatError::MissingSeparator)?;
        let address = address.trim().trim_end_matches('/');
        let token = token.trim();
        if address.is_empty() {
            return Err(KeyFormatError::MissingField("endereço"));
        }
        if token.is_empty() {
            return Err(KeyFormatError::MissingField("token"));
        }
        let url = if address.contains("://") {
            address.to_owned()
        } else {
            format!("{}://{address}", implied_scheme(address))
        };
        Ok(AccessKey {
            url,
            token: token.to_owned(),
        })
    }
}

/// Parte `<endereço>::<token>` no primeiro `::` que não seja do próprio
/// endereço — o esquema (`https://`) não tem `::`, mas um IPv6 entre
/// colchetes (`[::1]:9797`) tem, então a busca começa depois do `]`.
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

    fn key(url: &str, token: &str) -> AccessKey {
        AccessKey {
            url: url.into(),
            token: token.into(),
        }
    }

    fn decoded_payload(chave: &str) -> String {
        String::from_utf8(URL_SAFE_NO_PAD.decode(chave).unwrap()).unwrap()
    }

    #[test]
    fn formato_e_host_porta_dois_pontos_duplos_token() {
        let chave = key("http://rustploy.exemplo.com:9797", "a3f9").encode();
        assert_eq!(decoded_payload(&chave), "rustploy.exemplo.com:9797::a3f9");
        assert!(
            !chave.contains("rustploy.exemplo.com"),
            "endereço não pode ficar explícito"
        );
        assert_eq!(
            AccessKey::decode(&chave).unwrap(),
            key("http://rustploy.exemplo.com:9797", "a3f9")
        );
        assert_eq!(
            AccessKey::decode(&format!("  {chave}\n")).unwrap(),
            key("http://rustploy.exemplo.com:9797", "a3f9")
        );
    }

    #[test]
    fn esquema_so_vai_na_chave_quando_nao_da_para_deduzir() {
        for (url, payload) in [
            ("https://a.b", "a.b::t"),
            ("https://a.b:443", "a.b:443::t"),
            ("https://a.b:9797", "https://a.b:9797::t"),
            ("http://a.b:443", "http://a.b:443::t"),
            ("http://[::1]:9797", "[::1]:9797::t"),
        ] {
            let chave = key(url, "t").encode();
            assert_eq!(decoded_payload(&chave), payload, "{url}");
            assert_eq!(AccessKey::decode(&chave).unwrap(), key(url, "t"), "{url}");
        }
    }

    #[test]
    fn aceita_base64_padrao_com_padding() {
        // O que sai de `echo -n 'a.b:9797::t~?' | base64`.
        let chave = STANDARD.encode("a.b:9797::t~?");
        assert_eq!(
            AccessKey::decode(&chave).unwrap(),
            key("http://a.b:9797", "t~?")
        );
    }

    #[test]
    fn token_pode_conter_dois_pontos_duplos() {
        let chave = key("http://a.b:1", "x::y").encode();
        assert_eq!(AccessKey::decode(&chave).unwrap().token, "x::y");
    }

    #[test]
    fn cada_erro_diz_qual_parte_falhou() {
        assert_eq!(AccessKey::decode(""), Err(KeyFormatError::Empty));
        assert_eq!(AccessKey::decode("!!!"), Err(KeyFormatError::NotBase64));
        assert_eq!(
            AccessKey::decode(&URL_SAFE_NO_PAD.encode("so-o-token")),
            Err(KeyFormatError::MissingSeparator)
        );
        assert_eq!(
            AccessKey::decode(&URL_SAFE_NO_PAD.encode("a.b:1::")),
            Err(KeyFormatError::MissingField("token"))
        );
        assert_eq!(
            AccessKey::decode(&URL_SAFE_NO_PAD.encode("::t")),
            Err(KeyFormatError::MissingField("endereço"))
        );
    }
}
