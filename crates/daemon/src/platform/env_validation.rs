//! Validação das env vars gravadas por serviço, projeto e job: nome válido e
//! sem chave repetida. Valor vazio é legítimo (`FOO=`) e passa.

use shared::EnvVar;
use std::collections::HashSet;

/// Nome aceito: começa com letra ou `_`; o resto são letras, dígitos, `_`,
/// `.` e `-` (`.`/`-` aparecem em apps Spring e em templates já salvos).
fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Primeira violação, já em texto para o usuário; `None` se está tudo certo.
pub fn validate(env_vars: &[EnvVar]) -> Option<String> {
    let mut seen = HashSet::new();
    for env_var in env_vars {
        if !valid_key(&env_var.key) {
            return Some(format!(
                "nome de variável inválido: \"{}\" (use letras, dígitos e _, sem começar por dígito)",
                env_var.key
            ));
        }
        if !seen.insert(env_var.key.as_str()) {
            return Some(format!("variável repetida: {}", env_var.key));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::EnvVarValue;

    fn var(key: &str) -> EnvVar {
        EnvVar {
            key: key.to_string(),
            value: EnvVarValue::Plain(String::new()),
        }
    }

    #[test]
    fn accepts_valid_names_and_empty_values() {
        assert_eq!(
            validate(&[var("DATABASE_URL"), var("_X1"), var("a.b-c")]),
            None
        );
    }

    #[test]
    fn rejects_invalid_names() {
        for key in ["", "1ABC", "MINHA CHAVE", "A=B", "ÇÃO"] {
            assert!(validate(&[var(key)]).is_some(), "{key:?} deveria falhar");
        }
    }

    #[test]
    fn rejects_duplicate_keys() {
        let err = validate(&[var("A"), var("B"), var("A")]).unwrap();
        assert!(err.contains("repetida"), "{err}");
    }
}
