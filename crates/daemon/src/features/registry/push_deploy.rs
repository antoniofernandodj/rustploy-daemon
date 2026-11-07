//! Push-to-deploy: cada tag publicada no registry embutido redeploya os serviços que apontam para
//! ela e pediram isso (`ServiceSpec.registry_auto_deploy`). Fluxo de CI resultante:
//! `docker login` (token push) → `docker build` → `docker push` → o rustploy redeploya sozinho,
//! sem webhook HTTP nem Git no servidor. Ver `docs/plano-registry-embutido.md` (Fase 4).

use super::storage::PushedTag;
use crate::bootstrap::state::AppState;
use shared::{Event, Service, ServiceSource};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};

/// Repo e tag de uma referência de imagem do registry embutido, sem o host: de
/// `127.0.0.1:5100/app/api:prod` sai `("app/api", "prod")`. Sem tag explícita vale `latest` (como
/// o Docker). `None` se a imagem não aponta para o registry embutido — `is_embedded` já decide
/// isso (host/porta/domínio), então aqui só se descasca.
fn repo_and_tag(image: &str, is_embedded: bool) -> Option<(String, String)> {
    if !is_embedded {
        return None;
    }
    let (_host, rest) = image.split_once('/')?;
    // por digest (`repo@sha256:…`) não acompanha tag nenhuma: nunca é alvo de push-to-deploy
    if rest.contains('@') {
        return None;
    }
    match rest.rsplit_once(':') {
        // o `:` de uma porta não pode estar depois do último `/`; aqui `rest` já não tem host
        Some((repo, tag)) if !tag.contains('/') && !repo.is_empty() => {
            Some((repo.to_string(), tag.to_string()))
        }
        _ => Some((rest.to_string(), "latest".to_string())),
    }
}

/// Os serviços que devem redeployar com este push: opt-in ligado, fonte `Registry` apontando para
/// o registry embutido e casando repo **e** tag.
pub fn services_to_redeploy<'a>(
    services: &'a [Service],
    push: &PushedTag,
    port: u16,
    domain: Option<&str>,
) -> Vec<&'a Service> {
    services
        .iter()
        .filter(|s| s.spec.registry_auto_deploy)
        .filter(|s| match &s.spec.source {
            ServiceSource::Registry { image } => {
                let embedded = crate::features::deploy::executor::is_embedded_registry_image(
                    image, port, domain,
                );
                repo_and_tag(image, embedded).is_some_and(|(r, t)| r == push.repo && t == push.tag)
            }
            _ => false,
        })
        .collect()
}

/// Consome os pushes até o canal fechar: publica o evento para os clientes e dispara o deploy
/// dos serviços que casam.
pub async fn run(state: AppState, mut rx: UnboundedReceiver<PushedTag>) {
    while let Some(push) = rx.recv().await {
        state.bus.publish(Event::RegistryPush {
            repo: push.repo.clone(),
            tag: push.tag.clone(),
            digest: push.digest.clone(),
        });

        let services = match crate::store::services::list_all(&state.db).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "push-to-deploy: não consegui listar os serviços");
                continue;
            }
        };
        let port = crate::platform::config::RustployConfig::global()
            .registry
            .port;
        let mut domain = crate::platform::config::RustployConfig::global()
            .registry
            .domain
            .clone();
        if let Ok(Some(d)) = crate::store::daemon_settings::get(
            &state.db,
            crate::store::daemon_settings::KEY_REGISTRY_DOMAIN,
        )
        .await
            && !d.trim().is_empty()
        {
            domain = Some(d);
        }

        for svc in services_to_redeploy(&services, &push, port, domain.as_deref()) {
            info!(
                service = %svc.spec.name, repo = %push.repo, tag = %push.tag,
                "push-to-deploy: imagem nova no registry, disparando deploy"
            );
            let (ctx, id) = (state.deploy_ctx(), svc.id.clone());
            tokio::spawn(async move {
                crate::features::deploy::start::handle(ctx, id).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(repo: &str, tag: &str) -> PushedTag {
        PushedTag {
            repo: repo.into(),
            tag: tag.into(),
            digest: "sha256:abc".into(),
        }
    }

    /// Um `Service` mínimo, montado por JSON (os campos opcionais caem no `serde(default)`).
    fn svc(image: &str, auto: bool) -> Service {
        serde_json::from_value(serde_json::json!({
            "id": format!("svc_{image}"),
            "spec": {
                "name": "api", "project_id": "prj", "port": 80,
                "source": { "Registry": { "image": image } },
                "env_vars": [], "volumes": [], "domain": null, "host_port": null,
                "tls_enabled": false, "replicas": 1,
                "healthcheck": { "kind": "None", "interval_secs": 5, "timeout_secs": 3, "retries": 3, "start_period_secs": 0 },
                "resources": { "cpu_shares": 0, "mem_limit_bytes": 0 },
                "registry_auto_deploy": auto,
            },
            "status": "Stopped",
            "live_container_id": null,
            "created_at": "2026-11-07T00:00:00Z",
            "updated_at": "2026-11-07T00:00:00Z",
        }))
        .expect("Service de teste")
    }

    fn imagens(servicos: Vec<&Service>) -> Vec<String> {
        servicos
            .into_iter()
            .map(|s| match &s.spec.source {
                ServiceSource::Registry { image } => image.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn so_redeploya_quem_pediu_e_casa_repo_e_tag() {
        let todos = vec![
            svc("127.0.0.1:5100/app/api:prod", true),
            svc("127.0.0.1:5100/app/api:prod", false), // não pediu
            svc("127.0.0.1:5100/app/api:dev", true),   // outra tag
            svc("127.0.0.1:5100/app/web:prod", true),  // outro repo
            svc("registry.exemplo.com/app/api:prod", true), // domínio configurado
            svc("docker.io/app/api:prod", true),       // registry de fora
        ];
        let p = push("app/api", "prod");
        assert_eq!(
            imagens(services_to_redeploy(
                &todos,
                &p,
                5100,
                Some("registry.exemplo.com")
            )),
            [
                "127.0.0.1:5100/app/api:prod",
                "registry.exemplo.com/app/api:prod"
            ]
        );
        // sem domínio configurado, o do domínio deixa de casar
        assert_eq!(
            imagens(services_to_redeploy(&todos, &p, 5100, None)).len(),
            1
        );
    }

    #[test]
    fn extrai_repo_e_tag_sem_o_host() {
        assert_eq!(
            repo_and_tag("127.0.0.1:5100/app/api:prod", true),
            Some(("app/api".into(), "prod".into()))
        );
        assert_eq!(
            repo_and_tag("registry.exemplo.com/api", true),
            Some(("api".into(), "latest".into())),
            "sem tag vale latest"
        );
        assert_eq!(repo_and_tag("127.0.0.1:5100/api@sha256:aa", true), None);
        assert_eq!(repo_and_tag("docker.io/library/nginx:1", false), None);
    }
}
