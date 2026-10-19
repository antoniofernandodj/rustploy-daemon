//! Estágio `Staging`: sobe o container novo ao lado do live.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_staging(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let image = self.image_for(dep, svc);
        let network = self.network_name(&svc.spec.project_id).await?;
        let env = self.resolve_env(svc).await?;
        let replicas = svc.spec.replicas.max(1);
        let dep_short = self.short(&dep.id).to_string();

        // Antes de subir qualquer coisa: os nomes live que este deploy
        // vai ocupar não podem ser de outro serviço.
        self.ensure_live_names_free(svc).await?;

        if replicas == 1 {
            // Single replica: caminho existente, healthcheck e swap tratados nos próximos estados
            let cname = containers::replica_staging_name(&svc.id, &svc.spec.name, &dep_short, 0);
            self.log_step(&dep.id, &svc.id, "--> Criando container de staging")
                .await;
            info!(deployment_id = %dep.id, container_name = %cname, "step[Staging]: criando réplica única");
            let id = containers::create_staging(
                &self.docker.inner,
                &svc.spec,
                &image,
                &svc.id,
                &dep.id,
                &network,
                &env,
                &cname,
                0,
            )
            .await?;
            containers::start(&self.docker.inner, &id).await?;
            return Ok(DeployState::HealthcheckPolling);
        }

        // Multi-réplica: exige healthcheck configurado
        if svc.spec.healthcheck.kind == HealthcheckKind::None {
            return Err(anyhow!(
                "Deploy com múltiplas réplicas requer configuração de healthcheck"
            ));
        }

        // Rolling update: uma réplica por vez — sobe → healthcheck → derruba antiga → promove
        info!(
            deployment_id = %dep.id,
            replicas = replicas,
            "step[Staging/Rolling]: iniciando rolling update"
        );

        // Estado inicial: coleta IPs das réplicas live já existentes (None = primeiro deploy)
        let mut ips: Vec<Option<String>> = vec![None; replicas as usize];
        let lives = containers::find_live_replicas(&self.docker.inner, &svc.id).await?;
        for live in lives.iter().filter(|r| r.running && r.replica < replicas) {
            if ips[live.replica as usize].is_some() {
                continue;
            }
            if let Ok(ip) =
                containers::get_container_ip(&self.docker.inner, &live.id, &network).await
            {
                ips[live.replica as usize] = Some(ip);
            }
        }
        let alias = shared::app_network_alias(&svc.spec.name);

        for i in 0..replicas {
            let staging = containers::replica_staging_name(&svc.id, &svc.spec.name, &dep_short, i);
            info!(
                deployment_id = %dep.id,
                replica = i,
                container_name = %staging,
                "step[Staging/Rolling]: criando nova réplica"
            );

            let staging_id = containers::create_staging(
                &self.docker.inner,
                &svc.spec,
                &image,
                &svc.id,
                &dep.id,
                &network,
                &env,
                &staging,
                i,
            )
            .await?;
            containers::start(&self.docker.inner, &staging_id).await?;

            let ip =
                containers::get_container_ip(&self.docker.inner, &staging_id, &network).await?;
            info!(
                deployment_id = %dep.id,
                replica = i,
                ip = %ip,
                "step[Staging/Rolling]: verificando healthcheck da nova réplica"
            );
            // Falha aqui → RollingBack remove todos os stagings pendentes
            self.poll_healthcheck(&ip, &staging_id, svc, dep).await?;

            // Saudável: ganha o alias `rp_<safe>` na rede do projeto.
            // Reconecta o container, então o IP muda — relido abaixo,
            // antes de ir para o ingress (que ainda não o conhece).
            containers::attach_network_alias(&self.docker.inner, &staging_id, &network, &alias)
                .await?;
            let ip =
                containers::get_container_ip(&self.docker.inner, &staging_id, &network).await?;

            // Derruba a réplica live antiga (se existir) — pelo label;
            // pode estar no nome atual ou no legado.
            let olds = containers::find_live_replicas(&self.docker.inner, &svc.id).await?;
            for old in olds.iter().filter(|r| r.replica == i && r.id != staging_id) {
                info!(
                    deployment_id = %dep.id,
                    replica = i,
                    old_container = %old.id,
                    old_name = %old.name,
                    "step[Staging/Rolling]: parando réplica anterior"
                );
                let _ = containers::stop_graceful(&self.docker.inner, &old.id, 30).await;
                let _ = containers::remove(&self.docker.inner, &old.id).await;
            }

            // Promove staging → live
            let live_name = containers::replica_live_name(&svc.id, &svc.spec.name, i);
            containers::rename(&self.docker.inner, &staging_id, &live_name).await?;
            info!(
                deployment_id = %dep.id,
                replica = i,
                new_name = %live_name,
                "step[Staging/Rolling]: réplica promovida"
            );

            // Atualiza ingress com os IPs ativos até agora
            ips[i as usize] = Some(ip);
            let active: Vec<String> = ips.iter().flatten().cloned().collect();
            self.ingress.register_domains(&svc.spec, &active, &svc.id);
            if let Some(host_port) = svc.spec.host_port {
                let backends: Vec<String> = active
                    .iter()
                    .map(|ip| format!("{ip}:{}", svc.spec.port))
                    .collect();
                self.ingress.upsert_port_route(host_port, backends);
            }

            self.bus.publish(Event::DeployProgress {
                deployment_id: dep.id.clone(),
                service_id: svc.id.clone(),
                phase: "RollingUpdate".into(),
                percent: (((i + 1) as f32 / replicas as f32) * 100.0) as u8,
                description: format!("replica {}/{replicas} ok", i + 1),
            });
        }

        // Todas as réplicas substituídas; Promoting cuida do status no banco
        Ok(DeployState::Promoting)
    }
}
