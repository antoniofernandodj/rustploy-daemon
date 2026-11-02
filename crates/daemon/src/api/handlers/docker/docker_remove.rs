//! Remoção INDIVIDUAL de um recurso Docker (o par por-item dos `docker_prune`):
//! um container, imagem, volume ou rede específicos, escolhidos na aba Docker.
//! Sem `force` — o Docker recusa remover recursos em uso e o erro é propagado
//! ao cliente (a UI mostra a mensagem), evitando remoção acidental do que está
//! em uso. Invalida os caches de inventário afetados para a aba refletir na hora.

use crate::bootstrap::state::AppState;
use bollard::{
    container::RemoveContainerOptions, image::RemoveImageOptions, volume::RemoveVolumeOptions,
};
use shared::Response as RpResponse;

/// O serviço do estado a que um container pertence: pelo label `rustploy.service_id` (serviços
/// normais) ou pelo nome da stack Compose (`com.docker.compose.project`) — os containers de um
/// Compose não levam label do rustploy.
fn owning_service<'a>(
    labels: &std::collections::HashMap<String, String>,
    services: &'a [shared::Service],
) -> Option<&'a shared::Service> {
    services.iter().find(|s| {
        labels
            .get("rustploy.service_id")
            .is_some_and(|sid| crate::engine::containers::label_owned_by(sid, &s.id))
            || labels
                .get("com.docker.compose.project")
                .is_some_and(|cp| s.compose_project.as_deref() == Some(cp.as_str()))
    })
}

/// Para um container. **Recusa** o que ainda pertence a um serviço do estado (um serviço se para
/// pelo serviço; senão o watchdog o religaria): serve ao container **órfão** — o do serviço que
/// sumiu do estado por um commit externo ("removido no git") — que depois se remove com
/// [`remove_container`]. Funciona igual no Docker e no Podman.
pub async fn stop_container(state: AppState, id: String) -> RpResponse {
    let info = match state.docker.inner.inspect_container(&id, None).await {
        Ok(i) => i,
        Err(e) => return RpResponse::err("RuntimeError", e.to_string()),
    };
    let labels = info
        .config
        .as_ref()
        .and_then(|c| c.labels.clone())
        .unwrap_or_default();
    let services = crate::store::services::list_all(&state.db)
        .await
        .unwrap_or_default();
    let owner = owning_service(&labels, &services);
    if let Some(s) = owner {
        return RpResponse::err(
            "InvalidInput",
            format!(
                "este container pertence ao serviço '{}': pare-o pelo serviço (senão o watchdog o religa)",
                s.spec.name
            ),
        );
    }
    match state
        .docker
        .inner
        .stop_container(
            &id,
            Some(bollard::container::StopContainerOptions { t: 10 }),
        )
        .await
    {
        Ok(()) => {
            state.docker_cache.df.invalidate().await;
            RpResponse::Ok
        }
        Err(e) => RpResponse::err("RuntimeError", e.to_string()),
    }
}

/// `docker rm <id>` (sem `-f`): remove um container PARADO. `v: true` remove os
/// volumes anônimos associados (como o prune). Um container rodando é recusado
/// pelo Docker (409) — a UI só oferece "Remover" nos parados, mas o daemon não
/// força de qualquer forma.
pub async fn remove_container(state: AppState, id: String) -> RpResponse {
    let opts = RemoveContainerOptions {
        v: true,
        ..Default::default()
    };
    match state.docker.inner.remove_container(&id, Some(opts)).await {
        Ok(()) => {
            // Remover um container muda contagens de imagem em-uso e anexos de rede.
            state.docker_cache.df.invalidate().await;
            state.docker_cache.networks.invalidate().await;
            RpResponse::Ok
        }
        Err(e) => RpResponse::err("RuntimeError", e.to_string()),
    }
}

/// `docker rmi <id>` (sem `-f`): remove uma imagem sem uso. Uma imagem
/// referenciada por algum container é recusada pelo Docker.
///
/// Imagem com MAIS DE UMA tag (no Podman é comum: `localhost/x:t` e
/// `docker.io/library/x:t` juntas, depois de um `load`) é recusada por ID com
/// "more than one tag ... please force removal". Em vez de forçar (o que
/// atropelaria a proteção de imagem em uso), remove tag por tag: remover o
/// nome só desfaz a tag, e a última apaga a imagem — se um container a usa, essa
/// última remoção é recusada normalmente.
pub async fn remove_image(state: AppState, id: String) -> RpResponse {
    let result = match remove_image_by_name(&state, &id).await {
        Err(e) if needs_untag(&e) => remove_image_tag_by_tag(&state, &id).await,
        other => other,
    };
    state.docker_cache.df.invalidate().await;
    match result {
        Ok(()) => RpResponse::Ok,
        Err(e) => RpResponse::err("RuntimeError", e.to_string()),
    }
}

async fn remove_image_by_name(state: &AppState, name: &str) -> Result<(), bollard::errors::Error> {
    let opts = RemoveImageOptions {
        force: false,
        noprune: false,
    };
    state
        .docker
        .inner
        .remove_image(name, Some(opts), None)
        .await
        .map(|_| ())
}

async fn remove_image_tag_by_tag(state: &AppState, id: &str) -> Result<(), bollard::errors::Error> {
    let tags = state
        .docker
        .inner
        .inspect_image(id)
        .await?
        .repo_tags
        .unwrap_or_default();
    for tag in tags {
        remove_image_by_name(state, &tag).await?;
    }
    Ok(())
}

/// A recusa por ID de imagem com várias tags: o Podman diz "more than one tag",
/// o Docker "referenced in multiple repositories".
fn needs_untag(e: &bollard::errors::Error) -> bool {
    let msg = e.to_string();
    msg.contains("more than one tag") || msg.contains("multiple repositories")
}

/// `docker volume rm <name>` (sem `-f`): remove um volume sem uso.
pub async fn remove_volume(state: AppState, name: String) -> RpResponse {
    match state
        .docker
        .inner
        .remove_volume(&name, Some(RemoveVolumeOptions { force: false }))
        .await
    {
        Ok(()) => {
            state.docker_cache.df.invalidate().await;
            RpResponse::Ok
        }
        Err(e) => RpResponse::err("RuntimeError", e.to_string()),
    }
}

/// `docker network rm <id>`: remove uma rede sem uso. Uma rede com container
/// anexado (ou uma predefinida: bridge/host/none) é recusada pelo Docker.
pub async fn remove_network(state: AppState, id: String) -> RpResponse {
    match state.docker.inner.remove_network(&id).await {
        Ok(()) => {
            state.docker_cache.networks.invalidate().await;
            RpResponse::Ok
        }
        Err(e) => RpResponse::err("RuntimeError", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn svc(id: &str, compose: Option<&str>) -> shared::Service {
        let mut s = crate::features::wizard::app_spec(id.into(), "p".into());
        s.name = id.into();
        shared::Service {
            id: id.into(),
            spec: s,
            compose_project: compose.map(String::from),
            status: shared::ServiceStatus::Stopped,
            live_container_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn container_de_servico_vivo_nao_pode_ser_parado_por_aqui() {
        let services = vec![svc("svc_app", None), svc("svc_db", Some("rp_ab12cd34_db"))];
        // serviço normal, pelo label de id
        assert!(
            owning_service(&labels(&[("rustploy.service_id", "svc_app")]), &services).is_some()
        );
        // stack Compose (o mesmo rótulo no Docker e no Podman: o Rustploy usa o docker-compose nos dois)
        let o = owning_service(
            &labels(&[("com.docker.compose.project", "rp_ab12cd34_db")]),
            &services,
        );
        assert_eq!(o.map(|s| s.id.as_str()), Some("svc_db"));
        // órfão: o serviço sumiu do estado → ninguém é dono → pode parar
        assert!(
            owning_service(&labels(&[("rustploy.service_id", "svc_velho")]), &services).is_none()
        );
        assert!(
            owning_service(
                &labels(&[("com.docker.compose.project", "rp_outro")]),
                &services
            )
            .is_none()
        );
        assert!(owning_service(&labels(&[]), &services).is_none());
    }
}
