//! Dados de um serviço (volumes) que podem ser exportados e importados.
//! Ver `docs/plano-exportar-importar-dados-de-servico.md`.

use serde::{Deserialize, Serialize};

/// Uma parte do pacote de dados: hoje, um volume nomeado do serviço.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataPart {
    /// Nome lógico do volume (o do compose, sem o prefixo da stack): é o que viaja no pacote.
    pub name: String,
    /// `"volume"` (v1). Reservado para `"database"` (dump lógico), fase 2.
    pub kind: String,
    /// Tamanho em bytes, se o motor soube dizer (pode ser `None`).
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Resposta de `ServiceDataPlan`: o que um export levaria, sem mudar nada.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceDataPlan {
    pub service_name: String,
    pub parts: Vec<DataPart>,
    /// O serviço está rodando agora (o export com `stop=true` o para durante a cópia).
    pub running: bool,
    /// Por que não dá para exportar (ex.: serviço sem volumes). `None` = pode.
    pub blocked: Option<String>,
    /// Avisos para a tela mostrar (ex.: cópia no ar pode sair inconsistente).
    #[serde(default)]
    pub notes: Vec<String>,
    /// O export deve sair cifrado por padrão (há chave de recuperação configurada: só assim o
    /// pacote abre num servidor restaurado). Sem ela, o padrão é em claro.
    #[serde(default)]
    pub encrypt_default: bool,
}
