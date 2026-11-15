use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::*;

/// Database (e usuário dedicado) criado para um projeto dentro de um servidor
/// compartilhado. A senha nunca vai para o cliente: quem a usa é a env var do
/// projeto consumidor (e `connection_url`, já montada pelo daemon).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedDatabase {
    pub id: String,
    pub server_service_id: String,
    pub project_id: String,
    /// Nome do database **e** do usuário (`[a-z0-9_]`, ≤ 32).
    pub name: String,
    /// Env var do projeto que recebeu a connection string.
    pub env_var: String,
    pub connection_limit: Option<u32>,
    pub statement_timeout_ms: Option<u32>,
    /// `postgresql://usuario:senha@<alias>:5432/database` (como o projeto a vê).
    pub connection_url: String,
    pub created_at: DateTime<Utc>,
}

/// Pedido de novo database gerenciado (`Command::ManagedDatabaseCreate`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedDatabaseCreateReq {
    pub server_service_id: String,
    pub project_id: String,
    pub name: String,
    /// Vazio = padrão do motor (`DATABASE_URL`; Mongo: `MONGODB_URI`).
    #[serde(default)]
    pub env_var: String,
    /// Sobrescreve a env var se o projeto já a tiver (definida no projeto ou
    /// num serviço). Sem isto, a criação recusa.
    #[serde(default)]
    pub overwrite_env: bool,
    /// Não mexe em env var nenhuma (o assistente de migração troca a conexão
    /// só depois de copiar os dados; antes disso a app segue no banco antigo).
    #[serde(default)]
    pub skip_env: bool,
    #[serde(default)]
    pub connection_limit: Option<u32>,
    #[serde(default)]
    pub statement_timeout_ms: Option<u32>,
}

/// Um passo do assistente de migração.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MigrationStep {
    pub name: String,
    /// `pending` | `running` | `ok` | `failed` | `skipped`
    pub state: String,
    #[serde(default)]
    pub detail: String,
}

/// Migração de um banco antigo (serviço Compose do projeto) para um database
/// gerenciado de um servidor compartilhado. Ver `docs/plano-banco-compartilhado.md` §5.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Migration {
    pub id: String,
    pub project_id: String,
    pub source_service_id: String,
    pub source_database: String,
    pub dest_database_id: String,
    pub env_var: String,
    /// `Running` | `Failed` | `Completed` | `RolledBack` | `Discarded`
    pub status: String,
    pub steps: Vec<MigrationStep>,
    /// Saída do dump/restore e das verificações (as últimas linhas).
    #[serde(default)]
    pub log: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// Pedido de migração (`Command::MigrationStart`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MigrationStartReq {
    pub source_service_id: String,
    /// Nome do database **dentro** do banco antigo.
    pub source_database: String,
    /// Database gerenciado de destino (mesmo projeto, mesmo motor).
    pub dest_database_id: String,
    /// Env var que aponta para o banco e será trocada (vazio = padrão do motor).
    #[serde(default)]
    pub env_var: String,
}

/// Projeto autorizado a alcançar um servidor compartilhado.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SharedAccess {
    pub server_service_id: String,
    pub project_id: String,
    /// Nome da rede Docker do projeto (para a UI mostrar onde está conectado).
    #[serde(default)]
    pub network: String,
    /// Alias global pelo qual o projeto enxerga o servidor.
    #[serde(default)]
    pub alias: String,
}

/// Onde a chave de conexão estava e o que valia (`None` = não existia ali); guardado para desfazer a troca.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvChange {
    /// `None` = env do projeto; `Some(id)` = env desse serviço.
    pub service_id: Option<String>,
    pub key: String,
    pub previous: Option<EnvVarValue>,
}
