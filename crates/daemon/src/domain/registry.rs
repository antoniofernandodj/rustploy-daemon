use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Um repositório do registry embutido, para a lista da sub-aba Registry.
/// `size_bytes` é a soma dos manifests do repo (aproximação — blobs podem ser
/// compartilhados entre repos via dedupe, ver `docs/plano-registry-embutido.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryRepoInfo {
    pub name: String,
    pub tag_count: i64,
    pub size_bytes: i64,
    pub created_at: DateTime<Utc>,
}

/// Uma tag de um repositório, com o manifest que ela aponta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryTagInfo {
    pub tag: String,
    pub digest: String,
    pub media_type: String,
    pub size_bytes: i64,
    pub updated_at: DateTime<Utc>,
}

/// Retenção de tags do registry embutido: com ela ligada, antes de cada GC o
/// daemon apaga as tags mais antigas de cada repositório e mantém as `keep_last`
/// mais recentes (pela data de atualização da tag). Nunca apaga uma tag que um
/// serviço de fonte Registry usa. Só a linha da tag some; o manifest e as
/// camadas órfãs saem no GC.
///
/// Desligada por padrão: apagar dado sem o dono ter pedido seria surpresa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRetentionConfig {
    pub enabled: bool,
    /// Quantas tags mais recentes ficam por repositório (mínimo 1).
    pub keep_last: u32,
}

impl RegistryRetentionConfig {
    pub const DEFAULT_KEEP_LAST: u32 = 10;

    /// `keep_last` zero apagaria tudo o que não está em uso: vale no mínimo 1.
    pub fn effective_keep_last(&self) -> u32 {
        self.keep_last.max(1)
    }
}

impl Default for RegistryRetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_last: Self::DEFAULT_KEEP_LAST,
        }
    }
}

/// Status do registry embutido: config + agregados, para o cabeçalho da
/// sub-aba Registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryStatusInfo {
    pub enabled: bool,
    pub port: u16,
    pub domain: Option<String>,
    pub repo_count: i64,
    pub blob_count: i64,
    pub storage_bytes: i64,
    /// Política de retenção de tags. `serde(default)`: um daemon mais velho não
    /// manda o campo, e um cliente novo o lê como "desligada".
    #[serde(default)]
    pub retention: RegistryRetentionConfig,
}

/// Um token de acesso do registry (Basic auth) — o segredo em texto plano só
/// aparece uma vez, em `RegistryTokenCreated`; aqui é só metadado.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryTokenInfo {
    pub name: String,
    pub scope: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

// ── Jobs (tarefas one-shot via docker-compose, agendadas ou manuais) ──────────

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn padrao_vem_desligado_com_dez() {
        let c = RegistryRetentionConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.keep_last, 10);
    }

    #[test]
    fn keep_last_zero_vale_um() {
        let c = RegistryRetentionConfig {
            enabled: true,
            keep_last: 0,
        };
        assert_eq!(c.effective_keep_last(), 1);
    }

    #[test]
    fn status_antigo_sem_retention_le_como_desligada() {
        let j = r#"{"enabled":true,"port":5100,"domain":null,"repo_count":1,"blob_count":2,"storage_bytes":3}"#;
        let s: RegistryStatusInfo = serde_json::from_str(j).unwrap();
        assert!(!s.retention.enabled);
        assert_eq!(s.retention.keep_last, 10);
    }
}
