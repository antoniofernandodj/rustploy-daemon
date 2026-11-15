use serde::{Deserialize, Serialize};

use crate::domain::*;

/// Resolve as variáveis de ambiente para um serviço, combinando as do projeto e as do serviço.
/// As variáveis do serviço têm precedência sobre as do projeto.
pub fn resolve_env_vars(
    project: &Project,
    service: &Service,
) -> std::collections::HashMap<String, EnvVar> {
    let mut resolved = std::collections::HashMap::new();

    // Adiciona as variáveis de ambiente do projeto
    for env_var in &project.env_vars {
        resolved.insert(env_var.key.clone(), env_var.clone());
    }

    // Adiciona/sobrescreve com as variáveis de ambiente do serviço
    for env_var in &service.spec.env_vars {
        resolved.insert(env_var.key.clone(), env_var.clone());
    }

    resolved
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnvVar {
    pub key: String,
    pub value: EnvVarValue,
}

/// Uma linha de comentário do editor `.env`, ancorada à var que a segue (por
/// `key`, não por posição) — assim ela sobrevive a uma reordenação de vars.
/// `before_key: None` é um comentário solto/final, sem var associada.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EnvComment {
    /// Linha completa, incluindo o `#`.
    pub text: String,
    /// `#[serde(default)]`: um comentário final/solto (`before_key = nil`)
    /// vira uma tabela Lua sem essa chave (nil em table constructor nunca
    /// gera entrada) — sem isso, o JSON resultante omite a chave e a
    /// desserialização falha com "missing field".
    #[serde(default)]
    pub before_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EnvVarValue {
    Plain(String),
    Secret(String),
}
