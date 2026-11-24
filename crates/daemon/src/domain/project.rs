use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Variáveis de ambiente herdadas por todos os serviços deste projeto no deploy.
    #[serde(default)]
    pub env_vars: Vec<EnvVar>,
    /// Comentários (`# ...`) do editor `.env` do projeto, ancorados por `key`
    /// (mesmo esquema do `ServiceSpec.env_comments`). Não participam da
    /// construção do ambiente — só a lista/edição via UI olha pra isso.
    #[serde(default)]
    pub env_comments: Vec<EnvComment>,
    /// Este projeto é um **ambiente** (homologação, preview…) de outro: ver [`EnvironmentOf`].
    /// `None` = projeto comum (ou de origem).
    #[serde(default)]
    pub environment: Option<EnvironmentOf>,
    pub created_at: DateTime<Utc>,
}
