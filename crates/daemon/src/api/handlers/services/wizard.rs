//! Wizard "Novo serviço" server-side: catálogos (`WizardCatalog`) e criação
//! (`WizardCreate`). A lógica de montagem do `ServiceSpec` vive em
//! `crate::features::wizard` (que tem acesso aos blueprints de `crate::features::templates`); aqui
//! só ligamos aos handlers existentes.

use crate::bootstrap::state::AppState;
use crate::features::wizard;
use shared::Response as RpResponse;
use shared::WizardCreateReq;

/// Catálogos de bancos/brokers/templates prontos como JSON para o contexto do
/// cliente (`new_service_databases`/`new_service_brokers`/`new_service_templates`). `search` filtra os templates.
pub async fn catalog(search: String) -> RpResponse {
    RpResponse::WizardCatalog {
        dbs: wizard::db_rows_json(),
        brokers: wizard::broker_rows_json(),
        templates: wizard::templates_catalog_json(&search),
    }
}

/// Monta o `ServiceSpec` a partir dos campos coletados pelo wizard e cria o
/// serviço — reaproveitando o handler `service_create`.
pub async fn create(state: AppState, req: WizardCreateReq) -> RpResponse {
    match wizard::build_spec(&req) {
        Ok(spec) => crate::features::services::create::handle(state, spec).await,
        Err(e) => RpResponse::err("WizardError", e),
    }
}
