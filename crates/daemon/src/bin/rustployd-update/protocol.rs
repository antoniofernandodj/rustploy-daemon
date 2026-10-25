//! O protocolo do socket: uma linha JSON por pedido, uma de resposta. Só dois verbos.
//!
//! ```text
//! → {"op":"install","deb":"/var/lib/rustploy/updates/rustployd_0.2.0-1_amd64.deb",
//!    "sig":"/var/lib/rustploy/updates/rustployd_0.2.0-1_amd64.deb.minisig"}
//! ← {"ok":true,"version":"0.2.0-1"}        (aceito: assinatura e regras conferem; instalando)
//! ← {"ok":false,"error":"assinatura inválida: …"}
//!
//! → {"op":"rollback"}
//! ← {"ok":true}                            (restaurou os binários do backup)
//! ```
//!
//! O resultado final da instalação NÃO volta por aqui: o `dpkg` reinicia o daemon,
//! que é o cliente desta conexão. Ele aparece em `last.json` (ver `update_core::outcome`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Install { deb: String, sig: String },
    Rollback,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn accepted(version: Option<String>) -> Self {
        Self {
            ok: true,
            version,
            error: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            version: None,
            error: Some(error.into()),
        }
    }

    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| "{\"ok\":false}".into());
        line.push('\n');
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_verbs() {
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"op":"install","deb":"/a.deb","sig":"/a.deb.minisig"}"#
            )
            .unwrap(),
            Request::Install {
                deb: "/a.deb".into(),
                sig: "/a.deb.minisig".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"rollback"}"#).unwrap(),
            Request::Rollback
        );
    }

    #[test]
    fn unknown_verbs_and_missing_fields_do_not_parse() {
        for bad in [
            r#"{"op":"exec","cmd":"id"}"#,
            r#"{"op":"install"}"#,
            r#"{"op":"install","deb":"/a"}"#,
            r#"{}"#,
            "isto não é json",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_are_single_lines() {
        let accepted = Response::accepted(Some("0.2.0-1".into())).to_line();
        assert_eq!(accepted, "{\"ok\":true,\"version\":\"0.2.0-1\"}\n");
        let failed = Response::failed("não").to_line();
        assert_eq!(failed, "{\"ok\":false,\"error\":\"não\"}\n");
        assert_eq!(failed.matches('\n').count(), 1);
    }
}
