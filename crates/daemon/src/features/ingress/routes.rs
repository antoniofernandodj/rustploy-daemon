//! Rotas próprias (`domain::Route`): gravar no estado de infra, apagar, listar sem a senha e
//! compilar para o proxy. Ver `docs/arquivo/plano-releases-ambientes-rotas-volumes.md` §3.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::{Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use ulid::Ulid;

use crate::domain::{Route, RouteAction, normalize_route_domain, validate_route_shape};
use crate::features::ingress::router::{CustomAction, CustomRoute};
use crate::features::ingress::{IngressController, TlsManager};
use crate::platform::secrets::SecretsManager;
use crate::store::Db;

/// A rota como a tela recebe: a senha nunca sai do daemon.
fn without_password(mut route: Route) -> Route {
    if let Some(auth) = &mut route.basic_auth {
        auth.password = None;
    }
    route
}

/// Todas as rotas, de todos os projetos, por domínio.
pub fn list(db: &Db) -> Vec<Route> {
    let mut routes: Vec<Route> = db
        .infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|project| project.routes.iter().cloned())
        .map(without_password)
        .collect();
    routes.sort_by(|left, right| left.domain.cmp(&right.domain));
    routes
}

/// Cria (`id` vazio) ou atualiza uma rota. A senha vem em claro na ida e é gravada cifrada;
/// `password: None` numa atualização mantém a gravada.
pub fn save(db: &Db, secrets: &SecretsManager, mut route: Route) -> Result<Route> {
    route.domain = normalize_route_domain(&route.domain);
    validate_route_shape(&route).map_err(|cause| anyhow!(cause))?;
    let state = db.infra().snapshot();
    let project = state
        .projects
        .iter()
        .find(|project| project.id == route.project_id)
        .ok_or_else(|| anyhow!("projeto {} não encontrado", route.project_id))?;
    if let RouteAction::Forward { rules } = &route.action {
        for rule in rules {
            if !project
                .services
                .iter()
                .any(|service| service.id == rule.service_id)
            {
                bail!(
                    "o caminho {} aponta para um serviço que não é deste projeto",
                    rule.path_prefix
                );
            }
        }
    }
    for other_project in &state.projects {
        for other in &other_project.routes {
            if other.domain == route.domain && other.id != route.id {
                bail!("o domínio {} já tem uma rota", route.domain);
            }
        }
        for service in &other_project.services {
            if service
                .spec
                .domain_routes()
                .iter()
                .any(|domain| normalize_route_domain(&domain.domain) == route.domain)
            {
                bail!(
                    "o domínio {} já está no serviço {}: tire-o de lá antes de criar a rota",
                    route.domain,
                    service.spec.name
                );
            }
        }
    }
    let previous = state
        .projects
        .iter()
        .flat_map(|project| project.routes.iter())
        .find(|existing| !route.id.is_empty() && existing.id == route.id)
        .cloned();
    if !route.id.is_empty() && previous.is_none() {
        bail!("rota {} não encontrada", route.id);
    }
    if let Some(auth) = &mut route.basic_auth {
        match auth.password.take().filter(|password| !password.is_empty()) {
            Some(password) => auth.password = Some(secrets.encrypt(&password)?),
            None => {
                let kept = previous
                    .as_ref()
                    .and_then(|previous| previous.basic_auth.as_ref())
                    .and_then(|stored| stored.password.clone());
                auth.password = Some(kept.ok_or_else(|| anyhow!("informe a senha da rota"))?);
            }
        }
    }
    if route.id.is_empty() {
        route.id = format!("rte_{}", Ulid::new());
    }
    let stored = route.clone();
    db.infra().update(move |st| {
        for project in st.projects.iter_mut() {
            project.routes.retain(|existing| existing.id != stored.id);
        }
        let project = st
            .projects
            .iter_mut()
            .find(|project| project.id == stored.project_id)
            .ok_or_else(|| anyhow!("projeto {} não encontrado", stored.project_id))?;
        project.routes.push(stored);
        project
            .routes
            .sort_by(|left, right| left.domain.cmp(&right.domain));
        Ok(())
    })?;
    Ok(without_password(route))
}

pub fn delete(db: &Db, route_id: &str) -> Result<()> {
    db.infra().update(|st| {
        let mut found = false;
        for project in st.projects.iter_mut() {
            let before = project.routes.len();
            project.routes.retain(|route| route.id != route_id);
            found |= project.routes.len() != before;
        }
        if !found {
            bail!("rota {route_id} não encontrada");
        }
        Ok(())
    })
}

/// As rotas do estado compiladas para o proxy. Uma rota cuja senha não abre fica **fechada**
/// (pede uma senha que ninguém tem), nunca aberta.
pub fn compile(db: &Db, secrets: &SecretsManager) -> Vec<CustomRoute> {
    db.infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|project| project.routes.iter())
        .map(|route| {
            let basic_auth = route.basic_auth.as_ref().map(|auth| {
                let password = auth
                    .password
                    .as_deref()
                    .and_then(|encrypted| secrets.decrypt(encrypted).ok());
                match password {
                    Some(password) => {
                        format!("Basic {}", B64.encode(format!("{}:{password}", auth.user)))
                    }
                    None => {
                        tracing::warn!(domain = %route.domain, "rota: a senha não abriu; a rota fica fechada");
                        format!("Basic fechada-{}", Ulid::new())
                    }
                }
            });
            CustomRoute {
                domain: route.domain.clone(),
                tls: route.tls,
                action: match &route.action {
                    RouteAction::Forward { rules } => CustomAction::Forward(rules.clone()),
                    RouteAction::Redirect { to, permanent } => CustomAction::Redirect {
                        to: to.clone(),
                        permanent: *permanent,
                    },
                },
                basic_auth,
                cursor: Arc::new(AtomicUsize::new(0)),
            }
        })
        .collect()
}

/// Leva o estado das rotas para o proxy e pede o certificado das que têm TLS.
pub fn sync(db: &Db, secrets: &SecretsManager, ingress: &IngressController, tls: &Arc<TlsManager>) {
    ingress.set_custom_routes(compile(db, secrets));
    let state = db.infra().snapshot();
    for route in state
        .projects
        .iter()
        .flat_map(|project| project.routes.iter())
    {
        if route.tls {
            let tls = tls.clone();
            let domain = route.domain.clone();
            tokio::spawn(async move {
                if let Err(error) = tls.ensure_cert(&domain).await {
                    tracing::warn!(domain = %domain, error = %error, "TLS: falha ao provisionar certificado da rota");
                }
            });
        }
    }
}

/// Um domínio que o serviço quer usar já é de uma rota própria? (Para o `ServiceUpdate` recusar.)
pub fn domain_taken_by_route(db: &Db, domain: &str) -> bool {
    let domain = normalize_route_domain(domain);
    db.infra()
        .snapshot()
        .projects
        .iter()
        .flat_map(|project| project.routes.iter())
        .any(|route| route.domain == domain)
}
