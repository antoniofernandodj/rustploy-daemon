//! Verificação de versão: o que o daemon responde a `Command::CheckUpdate`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Resultado de comparar a versão do daemon com a última release publicada.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    /// Versão do daemon em execução (`CARGO_PKG_VERSION`).
    pub current_version: String,
    /// Versão da última release estável, sem o `v` da tag.
    pub latest_version: String,
    /// `true` só se `latest_version` for estritamente mais nova (semver).
    pub is_update_available: bool,
    /// Página da release (notas e arquivos).
    pub release_url: String,
    /// Quando a release foi publicada, se o GitHub informou.
    pub published_at: Option<DateTime<Utc>>,
    /// Quando o daemon consultou de fato (não quando respondeu: a resposta pode vir do cache).
    pub checked_at: DateTime<Utc>,
}

/// Em que ponto está a atualização do daemon pedida por `Command::ApplyUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateStage {
    /// Nenhuma atualização em curso (e nada a relatar).
    Idle,
    /// O daemon está baixando o pacote e a assinatura (e tirando o snapshot do banco).
    Downloading,
    /// O helper aceitou o pacote e está instalando; o daemon vai reiniciar.
    Installing,
    /// A pedido (`UpdateRollback`), o helper está restaurando os binários anteriores.
    RollingBack,
    /// Instalou e o serviço ficou estável.
    Succeeded,
    /// Foi recusada ou falhou antes de mudar o sistema (a causa está em `message`).
    Failed,
    /// Instalou, o serviço não ficou estável e os binários anteriores foram restaurados.
    RolledBack,
}

/// Resposta de `Command::UpdateStatus`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProgress {
    pub stage: UpdateStage,
    /// Versão que está sendo instalada (a da última release), quando há uma tentativa.
    pub target_version: Option<String>,
    /// Texto para a pessoa: o passo atual ou a causa de uma falha.
    pub message: String,
    /// `false` se o helper root de instalação não existe nesta máquina (instalação sem
    /// systemd, em container…): a UI esconde o botão e deixa só o comando manual.
    pub is_helper_available: bool,
    pub finished_at: Option<DateTime<Utc>>,
}
