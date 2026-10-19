//! `DeployExecutor`: roda um deployment pela máquina de estados
//! (clone/pull/build, staging, healthcheck, troca para live, rollback) e grava
//! cada passo no log de build.

use crate::platform::config::RustployConfig;
use crate::{
    engine,
    engine::{DockerClient, containers, images, networks},
    features::ingress::{IngressController, TlsManager},
    platform::event_bus::EventBus,
    platform::secrets::SecretsManager,
    store::Db,
};
use anyhow::{Result, anyhow};
use bollard::models::HealthStatusEnum;
use chrono::Utc;
use shared::{
    DeployState, Deployment, Event, HealthcheckKind, Service, ServiceSource, ServiceStatus,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

mod compose_flow;
mod drain;
mod healthcheck;
mod promote;
mod resolve;
mod rollback;
mod source;
mod staging;
mod state_machine;

#[cfg(test)]
use {
    rollback::{rollback_cause, truncate_chars},
    source::missing_dockerfile_msg,
    state_machine::failure_log_lines,
};

#[cfg(test)]
mod tests;

pub struct DeployExecutor {
    pub db: Arc<Db>,
    pub docker: Arc<DockerClient>,
    pub ingress: Arc<IngressController>,
    pub bus: Arc<EventBus>,
    pub secrets: Arc<SecretsManager>,
    pub tls: Arc<TlsManager>,
    pub db_path: PathBuf,
    pub drain_secs: u64,
    pub registry_internal_token: Option<Arc<str>>,
}

/// Reconhece se `image` aponta para o registry Docker embutido do próprio
/// rustployd, seja por loopback (`127.0.0.1:<port>`/`localhost:<port>`, caso
/// do deploy executor puxando no mesmo host) ou pelo domínio público
/// configurado (sem porta — o acesso externo passa pelo ingress, não fala
/// direto com a porta 5100).
fn is_embedded_registry_image(image: &str, port: u16, domain: Option<&str>) -> bool {
    if image.starts_with(&format!("127.0.0.1:{port}/")) {
        return true;
    }
    if image.starts_with(&format!("localhost:{port}/")) {
        return true;
    }
    if let Some(d) = domain
        && image.starts_with(&format!("{d}/"))
    {
        return true;
    }
    false
}

impl DeployExecutor {
    pub(super) fn clone_dir(&self, deployment_id: &str) -> PathBuf {
        self.db_path.join("builds").join(deployment_id)
    }

    pub(super) fn short<'a>(&self, id: &'a str) -> &'a str {
        let s = id.find('_').map(|i| &id[i + 1..]).unwrap_or(id);
        &s[..8.min(s.len())]
    }

    /// Falha o deploy se algum nome live que ele vai ocupar
    /// (`rp_<id8>_<safe>`, `…_r<i>`) já for de um container de **outro**
    /// serviço.
    ///
    /// Com o ID no nome isso não deveria acontecer — fica como defesa: sem
    /// esta checagem o deploy iria até o fim e só falharia no rename, com o
    /// live antigo deste serviço já removido. Melhor recusar antes de subir o
    /// staging, dizendo de quem é o nome. Ver
    /// `docs/plano-colisao-nome-container.md`.
    pub(super) async fn ensure_live_names_free(&self, svc: &Service) -> Result<()> {
        for i in 0..svc.spec.replicas.max(1) {
            let live = containers::replica_live_name(&svc.id, &svc.spec.name, i);
            let Some((cid, owner)) = containers::name_owner(&self.docker.inner, &live).await?
            else {
                continue;
            };
            if owner
                .as_deref()
                .is_some_and(|o| containers::label_owned_by(o, &svc.id))
            {
                continue;
            }
            let dono = match owner.as_deref() {
                Some(owner_id) => self.describe_service(owner_id).await,
                None => "um container que não foi criado pelo rustploy".to_string(),
            };
            warn!(
                service_id = %svc.id,
                container_name = %live,
                container_id = %cid,
                owner = ?owner,
                "deploy recusado: nome de container ocupado por outro serviço"
            );
            return Err(anyhow!(
                "o nome de container `{live}` já é usado por {dono}; o deploy foi \
                 recusado para não derrubá-lo. Nome de container é único no servidor \
                 inteiro: renomeie um dos dois serviços ou remova o container órfão"
            ));
        }
        Ok(())
    }

    /// "o serviço `api` do projeto `loja`", para mensagens de erro. Cai no ID
    /// quando o serviço não existe mais no banco (container órfão).
    pub(super) async fn describe_service(&self, service_id: &str) -> String {
        let candidates = [service_id.to_string(), format!("svc_{service_id}")];
        for id in &candidates {
            if let Ok(Some(owner)) = crate::store::services::get(&self.db, id).await {
                let projeto =
                    match crate::store::projects::get(&self.db, &owner.spec.project_id).await {
                        Ok(Some(p)) => p.name,
                        _ => owner.spec.project_id.clone(),
                    };
                return format!("o serviço `{}` do projeto `{projeto}`", owner.spec.name);
            }
        }
        format!("o serviço `{service_id}` (que não existe mais — container órfão)")
    }

    /// Garante a liberação da porta externa no firewall do host (helper
    /// `rustployd-fw`) e registra o resultado no deploy log. Idempotente — rodar
    /// a cada deploy também re-cria a regra caso o admin a tenha removido.
    /// Falha nunca aborta o deploy (pior caso = porta bloqueada, como antes).
    pub(super) async fn ensure_firewall(
        &self,
        deployment_id: &str,
        service_id: &str,
        host_port: u16,
    ) {
        let line = match crate::features::ingress::firewall::ensure_allowed(host_port).await {
            Ok(backend) if backend == "none" => {
                format!("--> Porta externa {host_port} exposta (nenhum firewall ativo no host)")
            }
            Ok(backend) => {
                format!("--> Porta externa {host_port} liberada no firewall ({backend})")
            }
            Err(e) => format!(
                "--> Aviso: não foi possível liberar a porta {host_port} no firewall: {e}. \
                 Se a conexão externa falhar, libere-a manualmente."
            ),
        };
        self.log_step(deployment_id, service_id, &line).await;
    }

    /// Persiste uma linha de log de build no banco e a publica no event bus.
    pub(super) async fn log_step(&self, deployment_id: &str, service_id: &str, line: &str) {
        let ts = chrono::Utc::now();
        let _ = crate::store::build_logs::append(&self.db, deployment_id, line, ts).await;
        self.bus.publish(Event::BuildLog {
            deployment_id: deployment_id.to_string(),
            service_id: service_id.to_string(),
            line: line.to_string(),
            timestamp: ts,
        });
    }

    /// Se `image` aponta pro registry embutido do próprio rustployd,
    /// devolve as credenciais do token interno `rp-internal` (ver
    /// `crate::features::registry::internal_token`) pra que o Docker Engine do host
    /// consiga se autenticar no pull — necessário desde que a Fase 2 do
    /// registry passou a exigir Basic auth em toda rota, inclusive loopback.
    pub(super) async fn registry_credentials_for(
        &self,
        image: &str,
    ) -> Option<bollard::auth::DockerCredentials> {
        let token = self.registry_internal_token.as_ref()?;
        let port = RustployConfig::global().registry.port;

        let mut domain = RustployConfig::global().registry.domain.clone();
        if let Ok(Some(d)) = crate::store::daemon_settings::get(
            &self.db,
            crate::store::daemon_settings::KEY_REGISTRY_DOMAIN,
        )
        .await
            && !d.trim().is_empty()
        {
            domain = Some(d);
        }

        if is_embedded_registry_image(image, port, domain.as_deref()) {
            Some(bollard::auth::DockerCredentials {
                username: Some("rp-internal".to_string()),
                password: Some(token.to_string()),
                ..Default::default()
            })
        } else {
            None
        }
    }

    pub(super) fn image_for(&self, dep: &Deployment, svc: &Service) -> String {
        match &svc.spec.source {
            ServiceSource::Registry { image } => image.clone(),
            ServiceSource::Git(_) => format!("rp_{}:{}", svc.spec.safe_name(), self.short(&dep.id)),
            ServiceSource::Archive(_) => {
                format!("rp_{}:{}", svc.spec.safe_name(), self.short(&dep.id))
            }
            ServiceSource::Compose(c) => format!("compose:{}", c.content),
        }
    }

    pub(super) fn archive_dir(&self, service_id: &str, archive_id: &str) -> PathBuf {
        self.db_path
            .join("uploads")
            .join("services")
            .join(service_id)
            .join(archive_id)
    }

    /// Nome da rede do projeto — o gravado em `project.network_name`.
    pub(super) async fn network_name(&self, project_id: &str) -> Result<String> {
        crate::store::projects::network_name(&self.db, project_id).await
    }

    pub(super) async fn ensure_network(&self, project_id: &str) -> Result<String> {
        let name = self.network_name(project_id).await?;
        networks::ensure_project_network(&self.docker.inner, &name).await
    }

    /// Wrapper fino: a lógica de verdade mora em `deploy::env_resolve::resolve`
    /// (reaproveitada pelo `JobRunner`, que precisa das mesmas env vars de
    /// base sem instanciar um `DeployExecutor` inteiro).
    pub(super) async fn resolve_env(&self, svc: &Service) -> Result<Vec<(String, String)>> {
        super::env_resolve::resolve(&self.db, &self.secrets, svc).await
    }

    pub(super) async fn load_deployment(&self, id: &str) -> Result<Deployment> {
        crate::store::deployments::get(&self.db, id)
            .await?
            .ok_or_else(|| anyhow!("deployment not found: {id}"))
    }

    pub(super) async fn load_service(&self, id: &str) -> Result<Option<Service>> {
        crate::store::services::get(&self.db, id).await
    }

    pub(super) async fn transition(
        &self,
        deployment_id: &str,
        from: &DeployState,
        to: DeployState,
        message: Option<String>,
    ) -> Result<()> {
        info!(
            deployment_id = %deployment_id,
            from = from.label(),
            to = to.label(),
            message = ?message,
            "executor: gravando transição no banco"
        );
        let dep = crate::store::deployments::transition(
            &self.db,
            deployment_id,
            from,
            to.clone(),
            message.clone(),
        )
        .await?;

        self.bus.publish(Event::DeployStateChanged {
            deployment_id: deployment_id.to_string(),
            service_id: dep.service_id.clone(),
            state: to.clone(),
            timestamp: Utc::now(),
            message,
        });
        info!(
            deployment_id = %deployment_id,
            state = to.label(),
            "executor: evento DeployStateChanged publicado"
        );
        Ok(())
    }
}
