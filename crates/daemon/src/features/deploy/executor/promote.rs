//! Estágios de promoção: troca do tráfego para o container novo e promoção a live.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_swapping_in(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let replicas = svc.spec.replicas.max(1);
        let dep_short = self.short(&dep.id).to_string();
        let net = self.network_name(&svc.spec.project_id).await?;

        // Coleta os IPs de todas as réplicas; cada rota de domínio
        // depois compõe `ip:porta` com a sua própria porta de container.
        let mut ips: Vec<String> = Vec::with_capacity(replicas as usize);
        let alias = shared::app_network_alias(&svc.spec.name);
        for i in 0..replicas {
            let staging = containers::replica_staging_name(&svc.id, &svc.spec.name, &dep_short, i);
            info!(
                deployment_id = %dep.id,
                replica = i,
                container_name = %staging,
                "step[SwappingIn]: resolvendo IP da réplica de staging"
            );
            let staging_id = containers::find_staging(
                &self.docker.inner,
                &svc.id,
                &svc.spec.name,
                &dep_short,
                i,
            )
            .await?
            .ok_or_else(|| anyhow!("staging container not found: {staging}"))?;
            // O alias `rp_<safe>` entra agora — depois do healthcheck e
            // **antes** de o ingress apontar para o staging: reconectar
            // muda o IP e derrubaria conexões já roteadas para ele.
            containers::attach_network_alias(&self.docker.inner, &staging_id, &net, &alias).await?;
            let ip = containers::get_container_ip(&self.docker.inner, &staging_id, &net).await?;
            ips.push(ip);
        }

        if !svc.spec.domain_routes().is_empty() {
            info!(
                deployment_id = %dep.id,
                domains = ?svc.spec.domain_routes().iter().map(|r| &r.domain).collect::<Vec<_>>(),
                ips = ?ips,
                "step[SwappingIn]: atualizando rotas de domínio no ingress"
            );
            self.ingress.register_domains(&svc.spec, &ips, &svc.id);
        }
        if let Some(host_port) = svc.spec.host_port {
            let backends: Vec<String> = ips
                .iter()
                .map(|ip| format!("{ip}:{}", svc.spec.port))
                .collect();
            info!(
                deployment_id = %dep.id,
                host_port,
                backends = ?backends,
                "step[SwappingIn]: atualizando rota de porta no ingress"
            );
            self.ingress.upsert_port_route(host_port, backends);
            self.ensure_firewall(&dep.id, &svc.id, host_port).await;
        }
        if svc.spec.domain_routes().is_empty() && svc.spec.host_port.is_none() {
            info!(
                deployment_id = %dep.id,
                "step[SwappingIn]: sem domínio nem porta externa, ingress não atualizado"
            );
        }

        // Para todas as instâncias antigas (suporte a replicas), excluindo as do deploy atual.
        match containers::find_old_containers(&self.docker.inner, &svc.id, &dep.id).await {
            Ok(old_ids) if !old_ids.is_empty() => {
                info!(
                    deployment_id = %dep.id,
                    count = old_ids.len(),
                    "step[SwappingIn]: parando instâncias live antigas"
                );
                for old in &old_ids {
                    let _ = containers::stop_graceful(&self.docker.inner, old, 30).await;
                }
                info!(deployment_id = %dep.id, "step[SwappingIn]: instâncias antigas paradas");
            }
            Ok(_) => {
                info!(deployment_id = %dep.id, "step[SwappingIn]: nenhuma instância live anterior");
            }
            Err(e) => {
                warn!(deployment_id = %dep.id, error = %e, "step[SwappingIn]: erro ao buscar containers antigos (ignorado)");
            }
        }
        Ok(DeployState::Draining)
    }

    pub(super) async fn step_promoting(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let replicas = svc.spec.replicas.max(1);
        let dep_short = self.short(&dep.id).to_string();
        info!(
            deployment_id = %dep.id,
            replicas = replicas,
            "step[Promoting]: promovendo staging → live"
        );

        // Remove todos os containers antigos (já parados no SwappingIn).
        match containers::find_old_containers(&self.docker.inner, &svc.id, &dep.id).await {
            Ok(old_ids) => {
                for old in &old_ids {
                    let _ = containers::remove(&self.docker.inner, old).await;
                }
                if !old_ids.is_empty() {
                    info!(deployment_id = %dep.id, count = old_ids.len(), "step[Promoting]: containers antigos removidos");
                }
            }
            Err(e) => {
                warn!(deployment_id = %dep.id, error = %e, "step[Promoting]: erro ao remover containers antigos (ignorado)");
            }
        }

        // Renomeia cada réplica de staging → live.
        let mut primary_id = String::new();
        for i in 0..replicas {
            let live = containers::replica_live_name(&svc.id, &svc.spec.name, i);
            let sid = match containers::find_staging(
                &self.docker.inner,
                &svc.id,
                &svc.spec.name,
                &dep_short,
                i,
            )
            .await?
            {
                Some(id) => id,
                None => {
                    warn!(deployment_id = %dep.id, replica = i, "step[Promoting]: réplica de staging não encontrada, pulando");
                    continue;
                }
            };
            // Live antigo desta réplica que `find_old_containers` não
            // achou (label de antes dos prefixos de ID, sem `svc_`):
            // ainda responderia pelo nome/alias e, se tiver o nome
            // atual, faria o rename falhar com conflito.
            let lives = containers::find_live_replicas(&self.docker.inner, &svc.id).await?;
            for stale in lives.iter().filter(|r| r.replica == i && r.id != sid) {
                info!(deployment_id = %dep.id, replica = i, container_id = %stale.id, name = %stale.name, "step[Promoting]: removendo live antigo remanescente");
                let _ = containers::remove(&self.docker.inner, &stale.id).await;
            }
            info!(
                deployment_id = %dep.id,
                replica = i,
                container_id = %sid,
                new_name = %live,
                "step[Promoting]: renomeando réplica"
            );
            containers::rename(&self.docker.inner, &sid, &live).await?;
            if i == 0 {
                primary_id = sid;
            }
        }

        info!(
            deployment_id = %dep.id,
            service_id = %svc.id,
            container_id = %primary_id,
            "step[Promoting]: atualizando status do serviço para Running"
        );
        crate::store::services::update_status(
            &self.db,
            &svc.id,
            &ServiceStatus::Running,
            if primary_id.is_empty() {
                None
            } else {
                Some(primary_id.as_str())
            },
        )
        .await?;
        self.bus.publish(Event::ServiceStatusChanged {
            service_id: svc.id.clone(),
            status: ServiceStatus::Running,
        });
        self.log_step(&dep.id, &svc.id, "==> Deploy concluído — serviço Running ✓")
            .await;
        info!(
            deployment_id = %dep.id,
            service_id = %svc.id,
            "step[Promoting]: serviço promovido para Running ✓"
        );

        let build_dir = self.clone_dir(&dep.id);
        if build_dir.exists() {
            let _ = std::fs::remove_dir_all(&build_dir);
            debug!(deployment_id = %dep.id, dir = %build_dir.display(), "step[Promoting]: diretório de build removido");
        }

        // Provisiona certificado TLS em background (não bloqueia o
        // pipeline) para cada domínio com TLS habilitado.
        for route in svc.spec.domain_routes().into_iter().filter(|r| r.tls) {
            let tls = self.tls.clone();
            let domain = route.domain.clone();
            tokio::spawn(async move {
                if let Err(e) = tls.ensure_cert(&domain).await {
                    warn!(domain = %domain, error = %e, "TLS: falha ao provisionar certificado");
                }
            });
        }

        // Transiciona qualquer deployment anterior em Live para Pruning
        // para evitar múltiplos registros Live para o mesmo serviço.
        if let Ok(history) =
            crate::store::deployments::list_for_service(&self.db, &svc.id, 20).await
        {
            for prev in history
                .iter()
                .filter(|d| d.id != dep.id && d.state == DeployState::Live)
            {
                let _ = crate::store::deployments::transition(
                    &self.db,
                    &prev.id,
                    &DeployState::Live,
                    DeployState::Pruning,
                    Some("superseded by newer deployment".into()),
                )
                .await;
                self.bus.publish(Event::DeployStateChanged {
                    deployment_id: prev.id.clone(),
                    service_id: svc.id.clone(),
                    state: DeployState::Pruning,
                    timestamp: Utc::now(),
                    message: Some("superseded".into()),
                });
            }
        }

        Ok(DeployState::Live)
    }
}
