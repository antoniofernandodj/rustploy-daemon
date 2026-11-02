//! Serialização YAML **determinística** do estado de infra.
//!
//! Dois exports do mesmo estado precisam gerar bytes idênticos, senão todo ciclo vira diff no git.
//! O `serde_yaml` já é determinístico para o mesmo valor; o que quebra isso é `HashMap`/`HashSet`
//! (ordem aleatória). Por isso os tipos do estado de infra usam `BTreeMap`/`BTreeSet` ou `Vec`
//! ordenado, e este módulo é o único ponto por onde o YAML passa (termina sempre com `\n`).

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

pub fn to_yaml<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut s = serde_yaml::to_string(value).context("serializando YAML")?;
    if !s.ends_with('\n') {
        s.push('\n');
    }
    Ok(s.into_bytes())
}

pub fn from_yaml<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_yaml::from_slice(bytes).context("lendo YAML")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Doc {
        name: String,
        env: BTreeMap<String, String>,
        ports: Vec<u16>,
    }

    fn doc(order: &[(&str, &str)]) -> Doc {
        Doc {
            name: "api".into(),
            env: order
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ports: vec![80, 443],
        }
    }

    #[test]
    fn mesma_coisa_gera_os_mesmos_bytes_independente_da_ordem_de_insercao() {
        let a = to_yaml(&doc(&[("Z", "1"), ("A", "2"), ("M", "3")])).unwrap();
        let b = to_yaml(&doc(&[("M", "3"), ("Z", "1"), ("A", "2")])).unwrap();
        assert_eq!(a, b);
        assert!(a.ends_with(b"\n"));
    }

    #[test]
    fn ida_e_volta() {
        let d = doc(&[("K", "v")]);
        let back: Doc = from_yaml(&to_yaml(&d).unwrap()).unwrap();
        assert_eq!(d, back);
    }
}
