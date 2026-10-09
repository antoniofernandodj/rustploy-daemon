//! `Command::ServiceExportPlan` e `Command::ServiceExport`: o lado de cá de
//! copiar um serviço para outro servidor (ver
//! `docs/plano-copiar-servico-entre-servidores.md`). O arquivo gerado é um
//! [`shared::ServiceBundle`]; quem o lê é `service_import`.

use crate::api::AppState;
use shared::{
    BundleOrigin, EnvVarValue, PlanVar, Project, Response as RpResponse, Service, ServiceBundle,
    ServiceExportPlan, ServiceSource, suggested_project_vars,
};
use std::collections::BTreeMap;
use tracing::{error, info};

/// Carrega o serviço e o projeto dele, ou já a resposta de erro.
async fn load(state: &AppState, service_id: &str) -> Result<(Service, Project), RpResponse> {
    let svc = match crate::db::services::get(&state.db, service_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(RpResponse::err("NotFound", "service not found")),
        Err(e) => return Err(RpResponse::err("DatabaseError", e.to_string())),
    };
    let project = match crate::db::projects::get(&state.db, &svc.spec.project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err(RpResponse::err("NotFound", "project not found")),
        Err(e) => return Err(RpResponse::err("DatabaseError", e.to_string())),
    };
    Ok((svc, project))
}

fn plan_var(key: &str, value: &EnvVarValue, suggested: bool) -> PlanVar {
    PlanVar {
        key: key.to_string(),
        is_secret: matches!(value, EnvVarValue::Secret(_)),
        suggested,
    }
}

/// O que dá para levar: as variáveis do serviço (todas vão) e as do projeto
/// (o usuário marca; as citadas pelo serviço vêm sugeridas). Nunca valores.
pub async fn plan(state: AppState, service_id: String) -> RpResponse {
    let (svc, project) = match load(&state, &service_id).await {
        Ok(x) => x,
        Err(r) => return r,
    };

    let blocked = matches!(svc.spec.source, ServiceSource::Archive(_)).then(|| {
        "Este serviço vem de um upload .zip, guardado neste servidor — não dá para exportá-lo. \
         Reenvie o zip no servidor de destino."
            .to_string()
    });

    let keys: Vec<String> = project.env_vars.iter().map(|e| e.key.clone()).collect();
    let suggested = suggested_project_vars(&svc.spec, &keys);

    RpResponse::ServiceExportPlan(ServiceExportPlan {
        service_name: svc.spec.name.clone(),
        project_name: project.name.clone(),
        service_env: svc
            .spec
            .env_vars
            .iter()
            .map(|e| plan_var(&e.key, &e.value, false))
            .collect(),
        project_env: project
            .env_vars
            .iter()
            .map(|e| plan_var(&e.key, &e.value, suggested.contains(&e.key)))
            .collect(),
        blocked,
    })
}

/// Gera o pacote. `project_env_keys` filtra as variáveis do projeto (só as
/// marcadas entram no arquivo); uma chave que o projeto não tem é erro, para
/// uma tela desatualizada não exportar menos do que o usuário viu.
pub async fn export(
    state: AppState,
    service_id: String,
    include_values: bool,
    project_env_keys: Vec<String>,
) -> RpResponse {
    let (svc, project) = match load(&state, &service_id).await {
        Ok(x) => x,
        Err(r) => return r,
    };

    let mut chosen = Vec::new();
    for key in &project_env_keys {
        match project.env_vars.iter().find(|e| &e.key == key) {
            Some(ev) => chosen.push(ev.clone()),
            None => {
                return RpResponse::err(
                    "UnknownProjectVar",
                    format!("o projeto '{}' não tem a variável '{key}'", project.name),
                );
            }
        }
    }

    let providers: BTreeMap<String, shared::GitProvider> =
        match crate::db::git_providers::list(&state.db).await {
            Ok(list) => list
                .into_iter()
                .map(|p| (p.id.clone(), p.to_public()))
                .collect(),
            Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
        };

    let origin = BundleOrigin {
        server: Some(state.public_base_url()),
        project: Some(project.name.clone()),
        exported_at: Some(chrono::Utc::now().to_rfc3339()),
        daemon: Some(env!("CARGO_PKG_VERSION").to_string()),
    };

    let bundle =
        match ServiceBundle::from_service(&svc, &providers, &chosen, include_values, origin) {
            Ok(b) => b,
            Err(msg) => return RpResponse::err("NotExportable", msg),
        };
    let yaml = match serde_yaml::to_string(&bundle) {
        Ok(y) => y,
        Err(e) => {
            error!(error = %e, "service_export: falha ao serializar o pacote");
            return RpResponse::err("SerializeError", e.to_string());
        }
    };

    info!(
        service = %svc.spec.name,
        project_vars = chosen.len(),
        include_values,
        "service_export: pacote gerado"
    );
    RpResponse::ServiceBundleYaml {
        yaml,
        filename: format!(
            "{}.rustploy-service.yml",
            shared::normalize_name(&svc.spec.name)
        ),
    }
}
