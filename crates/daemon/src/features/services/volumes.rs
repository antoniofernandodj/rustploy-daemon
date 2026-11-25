//! Montar e desmontar volumes de um serviço Application (`ServiceVolumeAttach`/`Detach`): o jeito de
//! "adotar" um volume órfão (os dados de um serviço apagado) ou dar um volume nomeado novo a um
//! serviço, sem mandar o spec inteiro. Vale no próximo deploy. Ver
//! `docs/arquivo/plano-releases-ambientes-rotas-volumes.md` §4.6.

use crate::domain::{Service, ServiceSource, VolumeMount, is_named_volume, validate_volume_mount};
use crate::engine::DockerClient;
use crate::store::Db;
use anyhow::{Result, anyhow, bail};

/// Outro serviço que já é dono do volume nomeado (pelo spec, pela etiqueta ou pela stack Compose).
async fn other_owner(
    db: &Db,
    docker: &DockerClient,
    service_id: &str,
    name: &str,
) -> Result<Option<String>> {
    let services = crate::store::services::list_all(db).await?;
    if let Some(other) = services.iter().find(|service| {
        service.id != service_id
            && service
                .spec
                .volumes
                .iter()
                .any(|volume| volume.host_path == name)
    }) {
        return Ok(Some(other.spec.name.clone()));
    }
    let Ok(volume) = docker.inner.inspect_volume(name).await else {
        return Ok(None);
    };
    let label_owner = volume
        .labels
        .get(crate::features::service_data::parts::SERVICE_LABEL)
        .and_then(|owner_id| services.iter().find(|service| &service.id == owner_id));
    let stack_owner = volume
        .labels
        .get("com.docker.compose.project")
        .and_then(|stack| {
            services.iter().find(|service| {
                matches!(service.spec.source, ServiceSource::Compose(_))
                    && &service.compose_project_name() == stack
            })
        });
    Ok(label_owner
        .or(stack_owner)
        .filter(|owner| owner.id != service_id)
        .map(|owner| owner.spec.name.clone()))
}

pub async fn attach(
    db: &Db,
    docker: &DockerClient,
    service_id: &str,
    mut mount: VolumeMount,
) -> Result<Service> {
    mount.host_path = mount.host_path.trim().to_string();
    mount.container_path = mount.container_path.trim().to_string();
    validate_volume_mount(&mount).map_err(|cause| anyhow!(cause))?;
    let service = crate::store::services::get(db, service_id)
        .await?
        .ok_or_else(|| anyhow!("service not found"))?;
    if matches!(service.spec.source, ServiceSource::Compose(_)) {
        bail!("serviço Compose declara os volumes no próprio arquivo: edite o compose");
    }
    if !is_named_volume(&mount.host_path)
        && !crate::features::service_data::parts::bind_allowed(&mount.host_path)
    {
        bail!(
            "a pasta {} não pode ser montada (é do sistema ou do próprio Rustploy)",
            mount.host_path
        );
    }
    if service.spec.volumes.iter().any(|volume| {
        volume.container_path.trim_end_matches('/') == mount.container_path.trim_end_matches('/')
    }) {
        bail!("o serviço já monta algo em {}", mount.container_path);
    }
    if is_named_volume(&mount.host_path)
        && let Some(owner) = other_owner(db, docker, service_id, &mount.host_path).await?
    {
        bail!(
            "o volume {} já é do serviço {owner}: dois serviços no mesmo volume corrompem os dados",
            mount.host_path
        );
    }
    let mut spec = service.spec.clone();
    spec.volumes.push(mount.clone());
    let updated = crate::store::services::update_spec(db, service_id, spec)
        .await?
        .ok_or_else(|| anyhow!("service not found"))?;
    if is_named_volume(&mount.host_path) {
        // Adotado: deixa de ser órfão.
        let _ = crate::store::orphan_volumes::forget(db, &mount.host_path).await;
    }
    Ok(updated)
}

/// Tira a montagem em `container_path`. Os dados ficam: o volume continua no servidor.
pub async fn detach(db: &Db, service_id: &str, container_path: &str) -> Result<Service> {
    let service = crate::store::services::get(db, service_id)
        .await?
        .ok_or_else(|| anyhow!("service not found"))?;
    let target = container_path.trim().trim_end_matches('/');
    let mut spec = service.spec.clone();
    let before = spec.volumes.len();
    spec.volumes
        .retain(|volume| volume.container_path.trim_end_matches('/') != target);
    if spec.volumes.len() == before {
        bail!("o serviço não monta nada em {container_path}");
    }
    crate::store::services::update_spec(db, service_id, spec)
        .await?
        .ok_or_else(|| anyhow!("service not found"))
}
