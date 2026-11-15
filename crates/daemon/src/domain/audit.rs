//! Auditoria de valores em claro no estado de infra (plano `docs/plano-backup-estado-em-git.md`, R6).

use serde::{Deserialize, Serialize};

/// Um valor que está (ou parece estar) em texto puro onde deveria estar cifrado. **Nunca** carrega
/// o valor: só onde ele está.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaintextFinding {
    /// `true` = o campo é sensível por natureza e devia estar cifrado (senha de banco gerenciado,
    /// webhook, secret…). `false` = suspeita pelo nome da variável / URL com senha / compose.
    pub definite: bool,
    /// Projeto (vazio para itens globais, como o `client_secret` de um provider).
    pub project: String,
    /// `serviço api`, `job backup`, `projeto`, `provider github`…
    pub scope: String,
    /// `env DB_PASSWORD`, `senha do banco loja`…
    pub what: String,
    /// Uma suspeita que o usuário marcou como "não é segredo" (não trava mais o git).
    #[serde(default)]
    pub allowed: bool,
}

/// Um arquivo da infra tal como o `state_git` o grava no repositório (resposta de
/// `Command::InfraDownload`). O conteúdo é sempre texto (YAML/JSON); os segredos vão como `ENC[age:…]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfraFile {
    /// Caminho relativo à raiz do repositório (`settings.yml`, `projects/<id>/project.yml`…).
    pub path: String,
    pub content: String,
}
