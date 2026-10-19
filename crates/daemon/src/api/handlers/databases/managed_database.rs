//! `Command::ManagedDatabase{List,Create,Delete}`: databases (com usuário
//! próprio) dentro de um servidor de banco compartilhado.

use crate::{
    bootstrap::state::AppState,
    db::{self, managed_database::Row},
    features::databases::env_switch,
    features::databases::{
        managed_view::to_view,
        shared_db::{self, Engine},
    },
    features::deploy::shared_net,
};
use chrono::Utc;
use shared::{ManagedDatabaseCreateReq, Response as RpResponse, Service};

async fn server(state: &AppState, id: &str) -> Result<(Service, Engine), RpResponse> {
    let svc = match db::services::get(&state.db, id).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(RpResponse::err("NotFound", "service not found")),
        Err(e) => return Err(RpResponse::err("DatabaseError", e.to_string())),
    };
    if svc.spec.shared.is_none() {
        return Err(RpResponse::err(
            "NotShared",
            "este serviço não é um servidor compartilhado",
        ));
    }
    let Some(engine) = Engine::from_kind(svc.spec.db_kind.as_deref()) else {
        return Err(RpResponse::err(
            "Unsupported",
            "motor de banco não suportado",
        ));
    };
    Ok((svc, engine))
}

async fn views(state: &AppState, svc: &Service, engine: Engine) -> RpResponse {
    match db::managed_database::list(&state.db, &svc.id).await {
        Ok(rows) => {
            RpResponse::ManagedDatabases(rows.iter().map(|r| to_view(svc, engine, r)).collect())
        }
        Err(e) => RpResponse::err("DatabaseError", e.to_string()),
    }
}

pub async fn list(state: AppState, server_service_id: String) -> RpResponse {
    match server(&state, &server_service_id).await {
        Ok((svc, engine)) => views(&state, &svc, engine).await,
        Err(r) => r,
    }
}

pub async fn create(state: AppState, req: ManagedDatabaseCreateReq) -> RpResponse {
    let (svc, engine) = match server(&state, &req.server_service_id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(e) = shared_db::validate_name(&req.name) {
        return RpResponse::err("InvalidName", e.to_string());
    }
    match db::projects::get(&state.db, &req.project_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return RpResponse::err("NotFound", "project not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    }
    let env_var = if req.env_var.trim().is_empty() {
        engine.default_env_var().to_string()
    } else {
        req.env_var.trim().to_string()
    };
    if !env_var
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return RpResponse::err("InvalidName", "nome de env var inválido");
    }
    let defined = if req.skip_env {
        Ok(false)
    } else {
        env_switch::is_defined(&state.db, &req.project_id, &env_var).await
    };
    match defined {
        Ok(true) if !req.overwrite_env => {
            return RpResponse::err(
                "EnvVarExists",
                format!(
                    "o projeto já tem a env var {env_var}; escolha outro nome ou confirme a sobrescrita"
                ),
            );
        }
        Ok(_) => {}
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    }
    if db::managed_database::list(&state.db, &svc.id)
        .await
        .is_ok_and(|l| l.iter().any(|r| r.name == req.name))
    {
        return RpResponse::err(
            "AlreadyExists",
            "já existe um database com esse nome neste servidor",
        );
    }
    let Some(cid) = shared_net::server_container(&state.docker.inner, &svc).await else {
        return RpResponse::err("NotRunning", "o servidor compartilhado não está no ar");
    };
    let password = match shared_db::generate_password() {
        Ok(p) => p,
        Err(e) => return RpResponse::err("Internal", e.to_string()),
    };

    // 1. Rede: o servidor entra na rede do projeto (antes de criar qualquer
    // coisa, para que um alias em conflito aborte sem efeito colateral).
    if req.project_id != svc.spec.project_id {
        if let Err(e) =
            shared_net::connect_to_project(&state.db, &state.docker, &svc, &cid, &req.project_id)
                .await
        {
            return RpResponse::err("NetworkError", e.to_string());
        }
        let _ = db::shared_access::grant(&state.db, &svc.id, &req.project_id).await;
    }

    // 2. Database + usuário no servidor (espera o init da imagem, se preciso).
    if let Err(e) = shared_db::wait_ready(engine, &cid, 30).await {
        return RpResponse::err("NotReady", e.to_string());
    }
    let script = shared_db::create_script(
        engine,
        &req.name,
        &password,
        req.connection_limit,
        req.statement_timeout_ms,
    );
    if let Err(e) = shared_db::exec(engine, &cid, &script).await {
        return RpResponse::err("ProvisionError", e.to_string());
    }

    // 3. Registro + env var do projeto.
    let row = Row {
        id: String::new(),
        server_service_id: svc.id.clone(),
        project_id: req.project_id.clone(),
        name: req.name.clone(),
        password,
        env_var: env_var.clone(),
        connection_limit: req.connection_limit,
        statement_timeout_ms: req.statement_timeout_ms,
        created_at: Utc::now(),
    };
    let row = match db::managed_database::insert(&state.db, &row).await {
        Ok(r) => r,
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    let url = to_view(&svc, engine, &row).connection_url;
    if !req.skip_env
        && let Err(e) = env_switch::switch(&state.db, &req.project_id, &env_var, &url).await
    {
        return RpResponse::err("DatabaseError", e.to_string());
    }
    views(&state, &svc, engine).await
}

pub async fn delete(state: AppState, id: String) -> RpResponse {
    let row = match db::managed_database::get(&state.db, &id).await {
        Ok(Some(r)) => r,
        Ok(None) => return RpResponse::err("NotFound", "database not found"),
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    let (svc, engine) = match server(&state, &row.server_service_id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(cid) = shared_net::server_container(&state.docker.inner, &svc).await else {
        return RpResponse::err("NotRunning", "o servidor compartilhado não está no ar");
    };
    if let Err(e) = shared_db::exec(engine, &cid, &shared_db::drop_script(engine, &row.name)).await
    {
        return RpResponse::err("ProvisionError", e.to_string());
    }
    if let Err(e) = db::managed_database::delete(&state.db, &id).await {
        return RpResponse::err("DatabaseError", e.to_string());
    }
    views(&state, &svc, engine).await
}

pub async fn list_all(state: AppState) -> RpResponse {
    let servers = match db::services::list_all(&state.db).await {
        Ok(v) => v,
        Err(e) => return RpResponse::err("DatabaseError", e.to_string()),
    };
    let mut out = vec![];
    for svc in servers.iter().filter(|s| s.spec.shared.is_some()) {
        let Some(engine) = Engine::from_kind(svc.spec.db_kind.as_deref()) else {
            continue;
        };
        if let Ok(rows) = db::managed_database::list(&state.db, &svc.id).await {
            out.extend(rows.iter().map(|r| to_view(svc, engine, r)));
        }
    }
    RpResponse::ManagedDatabases(out)
}
