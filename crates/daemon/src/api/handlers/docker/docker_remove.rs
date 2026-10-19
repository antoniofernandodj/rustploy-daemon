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
