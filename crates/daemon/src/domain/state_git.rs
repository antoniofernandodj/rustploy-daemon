//! Sincronização do estado de infra com um repositório git (plano
//! `docs/plano-backup-estado-em-git.md`, R6).

use serde::{Deserialize, Serialize};

/// Um commit do repositório de estado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateGitCommit {
    pub sha: String,
    /// RFC 3339.
    pub at: String,
    pub subject: String,
}

/// Algo que sumiu do estado por um commit externo. O daemon **não** apaga container nem dados por
/// causa disso: só marca, e a pessoa decide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateGitRemoved {
    /// `serviço:<id>` ou `projeto:<id>` (use em `StateGitDismissRemoval`).
    pub id: String,
    pub kind: String,
    pub name: String,
    /// Nome do projeto (vazio para um projeto).
    pub project: String,
    pub sha: String,
    pub at: String,
}

/// Um commit do remoto que o daemon **recusou** aplicar (e por quê).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateGitRejected {
    pub sha: String,
    pub reason: String,
}

/// Situação da sincronização, para a tela de Settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateGitStatus {
    /// `off` | `commit` | `sync`.
    pub mode: String,
    pub remote: Option<String>,
    pub branch: String,
    pub provider_id: Option<String>,
    /// O binário `git` existe no servidor?
    pub git_available: bool,
    /// `[secrets] recovery_recipient` está configurada? (sem ela, perder a chave perde os segredos)
    pub recovery_configured: bool,
    pub last_commit: Option<StateGitCommit>,
    /// Resultado do último `push` (`None` = nunca tentou).
    pub last_push_ok: Option<bool>,
    pub last_push_at: Option<String>,
    /// Último erro (git, rede, credencial). Não derruba nada: o daemon tenta de novo.
    pub last_error: Option<String>,
    /// Quantos valores em claro estão **impedindo** o commit agora (0 = livre).
    pub blocked_by_plaintext: usize,
    /// Há commits locais ainda não enviados.
    pub needs_push: bool,
    /// Último `fetch` (RFC 3339).
    #[serde(default)]
    pub last_fetch_at: Option<String>,
    /// Commits locais que o remoto não tem / commits do remoto que o local não tem.
    #[serde(default)]
    pub ahead: usize,
    #[serde(default)]
    pub behind: usize,
    /// `Some` = a sincronização está **pausada** (o histórico divergiu); retome com `StateGitResume`.
    #[serde(default)]
    pub paused_reason: Option<String>,
    /// O commit do remoto que foi recusado (YAML inválido, ciclo, valor em claro…).
    #[serde(default)]
    pub incoming_rejected: Option<StateGitRejected>,
    /// O que sumiu por um commit externo e espera decisão.
    #[serde(default)]
    pub removed_in_git: Vec<StateGitRemoved>,
    /// Serviços cujo declarado mudou por um commit externo: precisam de deploy para valer.
    #[serde(default)]
    pub pending_deploy: Vec<String>,
}
