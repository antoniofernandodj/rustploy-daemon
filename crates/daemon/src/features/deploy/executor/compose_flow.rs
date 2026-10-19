//! Caminho Compose: `compose up` da stack e espera do healthcheck.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_composing_up(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let ServiceSource::Compose(compose) = &svc.spec.source else {
            return Err(anyhow!("expected Compose source in ComposingUp"));
        };
        let project_name = svc.compose_project_name();
        info!(
            deployment_id = %dep.id,
            content_bytes = compose.content.len(),
            project = %project_name,
            "step[ComposingUp]: executando docker compose up"
        );
        let network_name = self.network_name(&svc.spec.project_id).await?;
        let env_vars = self.resolve_env(svc).await.unwrap_or_default();
        engine::compose::up(
            &self.docker.inner,
            &compose.content,
            &project_name,
            &svc.id,
            &dep.id,
            &network_name,
            &self.bus,
            &self.db,
            &env_vars,
            &self.clone_dir(&dep.id),
            self.registry_internal_token.clone(),
        )
        .await?;

        // Servidor de banco compartilhado: o container acabou de ser
        // (re)criado, e a recriação desfaz a conexão às redes dos
        // projetos autorizados — reconecta já, sem esperar o reconcile.
        if svc.spec.shared.is_some() {
            let n =
                crate::features::deploy::shared_net::sync_server(&self.db, &self.docker, svc).await;
            self.log_step(
                &dep.id,
                &svc.id,
                &format!("==> Servidor compartilhado conectado a {n} rede(s) de projeto"),
            )
            .await;
        }

        // Compose ingress: numa stack só UM container atende o domínio.
        // Ver `find_compose_ingress_container` — escolhe pelo
        // `ingress_service` do spec, senão por quem expõe a porta pedida.
        let live_container_id = containers::find_compose_ingress_container(
            &self.docker.inner,
            &project_name,
            compose.ingress_service.as_deref(),
            &svc.spec.ingress_container_ports(),
        )
        .await
        .ok()
        .flatten();

        if let Some(cid) = &live_container_id
            && let Ok(ip) =
                containers::get_container_ip(&self.docker.inner, cid, &network_name).await
        {
            self.wait_compose_healthcheck(&ip, svc, dep).await;
            let ips = vec![ip];
            if !svc.spec.domain_routes().is_empty() {
                info!(deployment_id = %dep.id, ?ips, "ComposingUp: registrando rotas de domínio");
                self.ingress.register_domains(&svc.spec, &ips, &svc.id);
            }
            if let Some(host_port) = svc.spec.host_port {
                let backend = format!("{}:{}", ips[0], svc.spec.port);
                info!(deployment_id = %dep.id, host_port, backend, "ComposingUp: registrando rota de porta");
                self.ingress.upsert_port_route(host_port, vec![backend]);
                self.ensure_firewall(&dep.id, &svc.id, host_port).await;
            }
        }

        self.log_step(
            &dep.id,
            &svc.id,
            "==> Compose up concluído — serviço Running ✓",
        )
        .await;
        info!(
            deployment_id = %dep.id,
            project = %project_name,
            container_id = ?live_container_id,
            "step[ComposingUp]: compose up concluído, promovendo serviço"
        );
        crate::db::services::update_status(
            &self.db,
            &svc.id,
            &ServiceStatus::Running,
            live_container_id.as_deref(),
        )
        .await?;
        self.bus.publish(Event::ServiceStatusChanged {
            service_id: svc.id.clone(),
            status: ServiceStatus::Running,
        });

        for route in svc.spec.domain_routes().into_iter().filter(|r| r.tls) {
            let tls = self.tls.clone();
            let domain = route.domain.clone();
            tokio::spawn(async move {
                if let Err(e) = tls.ensure_cert(&domain).await {
                    warn!(domain = %domain, error = %e, "TLS: falha ao provisionar certificado (compose)");
                }
            });
        }

        Ok(DeployState::Live)
    }
}
