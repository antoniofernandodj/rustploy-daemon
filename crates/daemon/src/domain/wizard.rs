use serde::{Deserialize, Serialize};

/// Campos coletados pelo wizard no cliente; o daemon (`WizardCreate`) resolve o
/// `kind`/`id` e monta o `ServiceSpec`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardCreateReq {
    /// "application" | "compose" | "database" | "broker" | "template".
    pub kind: String,
    /// db kind_id / broker kind_id / template id (vazio p/ application/compose).
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub app_name: String,
    pub db_name: String,
    pub user: String,
    pub password: String,
    pub root_password: String,
    pub image: String,
    pub use_replica: bool,
    pub template_values: Vec<String>,
    /// Quando true, o serviço nasce com `host_port = Some(0)` — a sentinela que
    /// faz o daemon alocar uma porta externa automaticamente (faixa
    /// `[external_ports]` da config) e liberá-la no firewall via `rustployd-fw`.
    pub expose_external: bool,
}
