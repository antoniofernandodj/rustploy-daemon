//! Estágio `RollingBack`: remove o staging e restaura o estado anterior, guardando a causa.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_rolling_back(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        self.log_step(&dep.id, &svc.id, "==> Deploy falhou — iniciando rollback")
            .await;
        if let ServiceSource::Compose(compose) = &svc.spec.source {
            let project_name = svc.compose_project_name();
            if compose_stack_was_touched(dep) {
                info!(
                    deployment_id = %dep.id,
                    project = %project_name,
                    "step[RollingBack]: derrubando compose stack"
                );
                let network_name = self.network_name(&svc.spec.project_id).await?;
                let env_vars = self.resolve_env(svc).await.unwrap_or_default();
                let _ = engine::compose::down(
                    &compose.content,
                    &project_name,
                    &network_name,
                    &env_vars,
                )
                .await;
            } else {
                // Falhou antes de mexer no stack (pré-deploy check, dependências): o que estava no
                // ar continua no ar. Derrubar aqui foi o que tirou o S3 do Chiquitos do ar em
                // 2026-11-20 — um job de pré-deploy falhou e o rollback apagou o stack que servia.
                info!(
                    deployment_id = %dep.id,
                    project = %project_name,
                    "step[RollingBack]: falhou antes do compose up; stack anterior preservado"
                );
                self.log_step(
                    &dep.id,
                    &svc.id,
                    "==> O stack que estava no ar não foi tocado (a falha foi antes do compose up)",
                )
                .await;
            }
            let err_status = ServiceStatus::Error(rollback_cause(dep));
            let _ =
                crate::store::services::update_status(&self.db, &svc.id, &err_status, None).await;
            self.bus.publish(Event::ServiceStatusChanged {
                service_id: svc.id.clone(),
                status: err_status,
            });
            return Ok(DeployState::Failed);
        }

        // Remove todos os containers de staging deste deployment.
        let replicas = svc.spec.replicas.max(1);
        let dep_short = self.short(&dep.id).to_string();
        info!(
            deployment_id = %dep.id,
            replicas = replicas,
            "step[RollingBack]: removendo containers de staging"
        );
        for i in 0..replicas {
            if let Ok(Some(id)) =
                containers::find_staging(&self.docker.inner, &svc.id, &svc.spec.name, &dep_short, i)
                    .await
            {
                // A evidência do deploy que falhou está no log deste container: guarda antes de apagar.
                crate::features::observability::container_logs::capture_final(
                    &self.docker.inner,
                    &svc.id,
                    &id,
                )
                .await;
                let _ = containers::remove(&self.docker.inner, &id).await;
                info!(deployment_id = %dep.id, replica = i, container_id = %id, "step[RollingBack]: staging removido");
            }
        }

        // Restaura todos os backends live anteriores para o ingress
        let live_replicas = svc.spec.replicas.max(1);
        let net = self.network_name(&svc.spec.project_id).await?;
        let live_ips =
            containers::live_replica_ips(&self.docker.inner, &svc.id, &net, live_replicas).await;
        if !live_ips.is_empty() {
            // Sempre: além dos domínios do serviço, o proxy guarda os IPs dele para as
            // rotas próprias (`domain::Route`) que apontam para ele.
            {
                info!(
                    deployment_id = %dep.id,
                    ips = ?live_ips,
                    "step[RollingBack]: restaurando rotas de domínio para lives anteriores"
                );
                self.ingress.register_domains(&svc.spec, &live_ips, &svc.id);
            }
            for (host_port, backends) in
                crate::features::ingress::external_ports::tcp_targets(&svc.spec, &live_ips)
            {
                info!(
                    deployment_id = %dep.id,
                    host_port,
                    "step[RollingBack]: restaurando rota de porta para lives anteriores"
                );
                self.ingress.upsert_port_route(host_port, backends);
            }
        } else {
            info!(deployment_id = %dep.id, "step[RollingBack]: nenhum live anterior para restaurar");
            for host_port in crate::features::ingress::external_ports::host_ports(&svc.spec) {
                self.ingress.remove_port_route(host_port);
            }
        }

        let err_status = ServiceStatus::Error(rollback_cause(dep));
        info!(
            deployment_id = %dep.id,
            service_id = %svc.id,
            "step[RollingBack]: atualizando serviço para Error"
        );
        crate::store::services::update_status(&self.db, &svc.id, &err_status, None).await?;
        self.bus.publish(Event::ServiceStatusChanged {
            service_id: svc.id.clone(),
            status: err_status,
        });
        info!(deployment_id = %dep.id, "step[RollingBack]: rollback concluído, estado = Failed");
        let _ = std::fs::remove_dir_all(self.clone_dir(&dep.id));
        Ok(DeployState::Failed)
    }
}

/// O deploy chegou a mexer no stack Compose (entrou em `ComposingUp`)? Antes disso — pré-deploy
/// check, resolução de dependências — nada do stack mudou, e o rollback não pode derrubá-lo.
pub(super) fn compose_stack_was_touched(dep: &Deployment) -> bool {
    dep.states_log
        .iter()
        .any(|t| t.to == DeployState::ComposingUp)
}

/// Texto que vai para `ServiceStatus::Error` quando um deploy cai em rollback.
///
/// Era a string fixa `"deploy failed"` — o mesmo problema do log mudo, num
/// outro lugar: o chip de status do serviço afirmava que algo falhou sem nunca
/// dizer o quê. A causa está no `states_log` do próprio deployment (a transição
/// que ENTROU em `RollingBack` carrega a mensagem gravada pelo `execute()`), e
/// aqui ela já foi persistida: o laço recarrega o deployment do banco a cada
/// iteração, então o `dep` deste step é posterior à transição.
///
/// Só a primeira linha, e truncada: este campo é renderizado como chip/linha
/// única na UI, não como log.
pub(super) fn rollback_cause(dep: &Deployment) -> String {
    dep.states_log
        .iter()
        .rev()
        .find(|t| t.to == DeployState::RollingBack)
        .and_then(|t| t.message.as_deref())
        .and_then(|m| m.lines().find(|l| !l.trim().is_empty()))
        .map(|primeira| truncate_chars(primeira.trim(), 160))
        .unwrap_or_else(|| "deploy failed".into())
}

/// Trunca por CARACTERE (não por byte): cortar um `&str` com índice de byte no
/// meio de um multibyte entra em pânico, e as mensagens aqui são em português.
pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cortado: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cortado.trim_end())
}
