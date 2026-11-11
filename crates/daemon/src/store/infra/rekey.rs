//! Recifra **tudo** o que está cifrado (`ENC[age:…]`) na infra para a identidade deste servidor e a chave
//! pública de recuperação. É o passo da recuperação de um servidor (`docs/plano-backup-de-dados-para-s3.md`,
//! Fase 4): o estado veio do git cifrado para o servidor antigo (+ a chave de recuperação); depois de
//! recifrado, o servidor novo o abre sozinho e a chave privada de recuperação pode ser descartada.
//!
//! O estado é percorrido como JSON e **qualquer** string no formato `ENC[age:…]` é recifrada, então um campo
//! cifrado novo, no futuro, entra sem ninguém lembrar de acrescentá-lo aqui. É tudo ou nada: se um só valor
//! não abrir, nada muda (e o erro diz qual).

use super::repo::Repo;
use crate::platform::age_format::is_enc;
use crate::platform::secrets::SecretsManager;
use anyhow::{Context, Result};
use serde_json::Value;

/// Recifra o que `f` conseguir; devolve quantos valores trocou. `Err` no primeiro que falhar.
fn walk(v: &mut Value, path: &str, f: &dyn Fn(&str) -> Result<String>) -> Result<usize> {
    match v {
        Value::String(s) if is_enc(s) => {
            *s = f(s).with_context(|| format!("recifrar `{path}`"))?;
            Ok(1)
        }
        Value::Array(items) => {
            let mut n = 0;
            for (i, item) in items.iter_mut().enumerate() {
                n += walk(item, &format!("{path}[{i}]"), f)?;
            }
            Ok(n)
        }
        Value::Object(map) => {
            let mut n = 0;
            for (k, item) in map.iter_mut() {
                n += walk(item, &format!("{path}.{k}"), f)?;
            }
            Ok(n)
        }
        // um JSON embutido em texto (ex.: o destino S3 em `settings`, guardado como string)
        Value::String(s) if s.trim_start().starts_with('{') && s.contains("ENC[age:") => {
            if let Ok(mut inner) = serde_json::from_str::<Value>(s) {
                let n = walk(&mut inner, path, f)?;
                *s = serde_json::to_string(&inner)?;
                Ok(n)
            } else {
                Ok(0)
            }
        }
        _ => Ok(0),
    }
}

/// Recifra toda a infra. Devolve quantos valores trocou.
pub fn rekey_all(repo: &Repo, secrets: &SecretsManager) -> Result<usize> {
    if !repo.is_initialized()? {
        return Ok(0);
    }
    repo.update(|st| {
        let mut json = serde_json::to_value(&*st)?;
        let n = walk(&mut json, "$", &|s| secrets.rekey(s))?;
        if n > 0 {
            *st = serde_json::from_value(json)
                .context("a infra recifrada não voltou a ser válida")?;
        }
        Ok(n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percorre_aninhado_e_json_embutido_e_ignora_o_resto() {
        let marca = |s: &str| Ok(format!("NOVO({s})"));
        let mut v = serde_json::json!({
            "a": "texto comum",
            "b": ["ENC[age:x]", 5, {"c": "ENC[age:y]"}],
            "d": "{\"secret_key\":\"ENC[age:z]\",\"bucket\":\"bk\"}",
            "e": "{isto não é json ENC[age:w]",
        });
        let n = walk(&mut v, "$", &marca).unwrap();
        assert_eq!(n, 3);
        assert_eq!(v["a"], "texto comum");
        assert_eq!(v["b"][0], "NOVO(ENC[age:x])");
        assert_eq!(v["b"][2]["c"], "NOVO(ENC[age:y])");
        let inner: Value = serde_json::from_str(v["d"].as_str().unwrap()).unwrap();
        assert_eq!(inner["secret_key"], "NOVO(ENC[age:z])");
        assert_eq!(inner["bucket"], "bk");
        assert_eq!(
            v["e"], "{isto não é json ENC[age:w]",
            "JSON inválido fica como está"
        );
    }

    #[test]
    fn um_valor_que_nao_abre_aborta_tudo_e_diz_qual() {
        let falha = |s: &str| {
            if s.ends_with("ruim]") {
                anyhow::bail!("não abre")
            }
            Ok("ok".to_string())
        };
        let mut v = serde_json::json!({"x": ["ENC[age:bom]", "ENC[age:ruim]"]});
        let e = format!("{:#}", walk(&mut v, "$", &falha).unwrap_err());
        assert!(e.contains("$.x[1]") && e.contains("não abre"), "{e}");
    }

    #[tokio::test]
    async fn recifra_a_infra_inteira_e_os_valores_passam_a_abrir_so_com_o_servidor_novo() {
        let r = age::x25519::Identity::generate();
        let a = SecretsManager::with_keys(
            std::sync::Arc::new(crate::store::Db::lazy_for_test()),
            age::x25519::Identity::generate(),
            Some(r.to_public()),
        )
        .unwrap();
        let b = SecretsManager::with_keys(
            std::sync::Arc::new(crate::store::Db::lazy_for_test()),
            age::x25519::Identity::generate(),
            None,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open(dir.path().join("infra")).unwrap();
        let antigo = a.encrypt("valor-secreto").unwrap();
        repo.update(|st| {
            st.settings.backup_destination =
                Some(serde_json::json!({"bucket": "bk", "secret_key": antigo.clone()}));
            Ok(())
        })
        .unwrap();

        use age::secrecy::ExposeSecret;
        b.begin_recovery(r.to_string().expose_secret()).unwrap();
        assert_eq!(rekey_all(&repo, &b).unwrap(), 1);
        b.end_recovery();

        let st = repo.snapshot();
        let dest = st.settings.backup_destination.clone().unwrap();
        let novo = dest["secret_key"].as_str().unwrap();
        assert_ne!(novo, antigo);
        assert_eq!(b.decrypt(novo).unwrap(), "valor-secreto");
        assert_eq!(dest["bucket"], "bk");
        // uma segunda passada também funciona (B já abre os valores): recifrar é repetível
        assert_eq!(rekey_all(&repo, &b).unwrap(), 1);
    }
}
