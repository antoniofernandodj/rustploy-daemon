//! Estágio de healthcheck: espera o staging (ou a stack Compose) ficar saudável.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_healthcheck_polling(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let staging = containers::staging_name(&svc.id, &svc.spec.name, self.short(&dep.id));
        info!(
            deployment_id = %dep.id,
            container_name = %staging,
            "step[HealthcheckPolling]: buscando container de staging"
        );
        let cid = containers::find_staging(
            &self.docker.inner,
            &svc.id,
            &svc.spec.name,
            self.short(&dep.id),
            0,
        )
        .await?
        .ok_or_else(|| anyhow!("staging container not found"))?;
        let net = self.network_name(&svc.spec.project_id).await?;
        // Um container que já morreu é retirado da rede pelo motor: pedir o IP falharia com
        // "container não encontrado na rede", e o rollback o removeria sem ninguém ver por que
        // ele morreu. Confere antes e, se for o caso, guarda a saída dele no build log.
        let inspect = containers::inspect(&self.docker.inner, &cid).await?;
        if !inspect
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false)
        {
            let exit_code = inspect.state.as_ref().and_then(|s| s.exit_code);
            return Err(self
                .report_stopped_container(dep, svc, &cid, exit_code, "antes do healthcheck")
                .await);
        }
        info!(
            deployment_id = %dep.id,
            container_id = %cid,
            network = %net,
            "step[HealthcheckPolling]: obtendo IP do container"
        );
        let ip = containers::get_container_ip(&self.docker.inner, &cid, &net).await?;
        info!(
            deployment_id = %dep.id,
            container_id = %cid,
            ip = %ip,
            port = svc.spec.port,
            healthcheck = ?svc.spec.healthcheck.kind,
            "step[HealthcheckPolling]: iniciando polling de healthcheck"
        );
        // Com `HealthcheckKind::None` nada é testado (ver
        // `poll_healthcheck`), então o log não pode dizer que esperou a
        // porta e que ela respondeu: um serviço cuja porta do spec não
        // é a que o container escuta ia para `Live` com duas linhas
        // verdes no log, e só aparecia como 502 no domínio.
        if svc.spec.healthcheck.kind == HealthcheckKind::None {
            self.log_step(
                &dep.id,
                &svc.id,
                &format!(
                    "--> Healthcheck desligado: {}:{} não será testada",
                    ip, svc.spec.port
                ),
            )
            .await;
            if !svc.spec.domain_routes().is_empty() || !svc.spec.tcp_routes().is_empty() {
                self.log_step(
                    &dep.id,
                    &svc.id,
                    "    (porta errada aqui só aparece como 502 no domínio — \
                     considere um healthcheck TCP)",
                )
                .await;
            }
            self.poll_healthcheck(&ip, &cid, svc, dep).await?;
        } else {
            self.log_step(
                &dep.id,
                &svc.id,
                &format!("--> Healthcheck: aguardando {ip}:{}", svc.spec.port),
            )
            .await;
            self.poll_healthcheck(&ip, &cid, svc, dep).await?;
            self.log_step(&dep.id, &svc.id, "--> Healthcheck OK").await;
        }
        info!(
            deployment_id = %dep.id,
            ip = %ip,
            "step[HealthcheckPolling]: healthcheck passou"
        );
        Ok(DeployState::SwappingIn)
    }

    /// Espera o container de staging ficar saudável (healthcheck do spec ou o
    /// HEALTHCHECK nativo da imagem) e, se falhar, guarda as últimas linhas de
    /// log dele antes do rollback removê-lo.
    /// Espera, com tempo limitado, o healthcheck TCP/HTTP do serviço Compose
    /// responder antes de promovê-lo a `Running`. Sem isto o `compose up`
    /// devolve assim que o container sobe, a porta ainda está fechada, e o
    /// watchdog marca `Degraded` na primeira checagem.
    ///
    /// Só informa, nunca falha o deploy: numa stack Compose a porta do spec
    /// pode não ser a que o healthcheck espera, e reprovar aqui quebraria
    /// deploys que hoje funcionam. Estourar o prazo vira uma linha no log e o
    /// watchdog segue dono do veredito depois.
    pub(super) async fn wait_compose_healthcheck(
        &self,
        ip: &str,
        svc: &crate::domain::Service,
        dep: &crate::domain::Deployment,
    ) {
        let hc = &svc.spec.healthcheck;
        if !matches!(hc.kind, HealthcheckKind::Tcp | HealthcheckKind::Http { .. }) {
            return;
        }
        let port = svc.spec.port;
        let timeout = std::time::Duration::from_secs(hc.timeout_secs as u64);
        let interval = std::time::Duration::from_secs((hc.interval_secs as u64).max(1));
        self.log_step(
            &dep.id,
            &svc.id,
            &format!("--> Healthcheck: aguardando {ip}:{port}"),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(hc.start_period_secs as u64)).await;
        for attempt in 1..=hc.retries.max(1) {
            let ok = match &hc.kind {
                HealthcheckKind::Http {
                    path,
                    expected_status,
                } => {
                    crate::platform::health::check_http(
                        &format!("http://{ip}:{port}{path}"),
                        *expected_status,
                        timeout,
                    )
                    .await
                }
                _ => crate::platform::health::check_tcp(&format!("{ip}:{port}"), timeout).await,
            };
            if ok {
                self.log_step(&dep.id, &svc.id, "--> Healthcheck OK").await;
                return;
            }
            if attempt < hc.retries.max(1) {
                tokio::time::sleep(interval).await;
            }
        }
        self.log_step(
            &dep.id,
            &svc.id,
            &format!("--> Healthcheck não respondeu em {ip}:{port} no prazo; seguindo (o watchdog continua conferindo)"),
        )
        .await;
    }

    /// O container de staging parou: grava as últimas linhas dele no build log (antes do
    /// rollback removê-lo) e devolve o erro do deploy, com o exit code e a última linha.
    pub(super) async fn report_stopped_container(
        &self,
        dep: &Deployment,
        svc: &Service,
        container_id: &str,
        exit_code: Option<i64>,
        quando: &str,
    ) -> anyhow::Error {
        error!(
            deployment_id = %dep.id,
            container_id = %container_id,
            exit_code = ?exit_code,
            quando,
            "healthcheck: container parou inesperadamente"
        );
        let crash_logs = containers::get_container_logs(&self.docker.inner, container_id, 50).await;
        if crash_logs.is_empty() {
            self.log_step(&dep.id, &svc.id, "  [sem output do container]")
                .await;
        } else {
            self.log_step(&dep.id, &svc.id, "--- output do container ---")
                .await;
            for line in &crash_logs {
                self.log_step(&dep.id, &svc.id, line).await;
            }
            self.log_step(&dep.id, &svc.id, "--------------------------")
                .await;
        }
        anyhow!(
            "{}",
            stopped_container_message(exit_code, quando, crash_logs.last().map(String::as_str))
        )
    }

    pub(super) async fn poll_healthcheck(
        &self,
        ip: &str,
        container_id: &str,
        svc: &Service,
        dep: &Deployment,
    ) -> Result<()> {
        let hc = &svc.spec.healthcheck;
        info!(
            deployment_id = %dep.id,
            kind = ?hc.kind,
            start_period = hc.start_period_secs,
            interval = hc.interval_secs,
            timeout = hc.timeout_secs,
            retries = hc.retries,
            "healthcheck: aguardando start_period antes do primeiro check"
        );
        sleep(Duration::from_secs(hc.start_period_secs as u64)).await;

        let interval = Duration::from_secs(hc.interval_secs as u64);
        let timeout = Duration::from_secs(hc.timeout_secs as u64);
        let max = hc.retries;

        for attempt in 0..max {
            info!(
                deployment_id = %dep.id,
                attempt = attempt + 1,
                max = max,
                "healthcheck: tentativa"
            );

            let inspect = containers::inspect(&self.docker.inner, container_id).await?;
            let running = inspect
                .state
                .as_ref()
                .and_then(|s| s.running)
                .unwrap_or(false);

            if !running {
                let exit_code = inspect.state.as_ref().and_then(|s| s.exit_code);
                return Err(self
                    .report_stopped_container(
                        dep,
                        svc,
                        container_id,
                        exit_code,
                        "durante o healthcheck",
                    )
                    .await);
            }

            let ok = match &hc.kind {
                HealthcheckKind::None => return Ok(()),
                HealthcheckKind::Http {
                    path,
                    expected_status,
                } => {
                    let url = format!("http://{ip}:{}{path}", svc.spec.port);
                    debug!(deployment_id = %dep.id, url = %url, expected = expected_status, "healthcheck: HTTP check");
                    crate::platform::health::check_http(&url, *expected_status, timeout).await
                }
                HealthcheckKind::Tcp => {
                    let addr = format!("{ip}:{}", svc.spec.port);
                    debug!(deployment_id = %dep.id, addr = %addr, "healthcheck: TCP check");
                    crate::platform::health::check_tcp(&addr, timeout).await
                }
                HealthcheckKind::DockerNative => {
                    let status = inspect
                        .state
                        .as_ref()
                        .and_then(|s| s.health.as_ref())
                        .and_then(|h| h.status.as_ref());
                    debug!(deployment_id = %dep.id, health_status = ?status, "healthcheck: DockerNative check");
                    // None  → imagem sem HEALTHCHECK; container rodando = ok
                    // HEALTHY → passou
                    // STARTING → ainda aquecendo, aguardar
                    // UNHEALTHY → falha explícita
                    match status {
                        None => true,
                        Some(s) => *s == HealthStatusEnum::HEALTHY,
                    }
                }
            };

            if ok {
                info!(
                    deployment_id = %dep.id,
                    attempt = attempt + 1,
                    "healthcheck: passou ✓"
                );
                return Ok(());
            }

            warn!(
                deployment_id = %dep.id,
                attempt = attempt + 1,
                max = max,
                "healthcheck: falhou nesta tentativa, aguardando próxima"
            );

            self.bus.publish(Event::DeployProgress {
                deployment_id: dep.id.clone(),
                service_id: svc.id.clone(),
                phase: "HealthcheckPolling".into(),
                percent: ((attempt as f32 / max as f32) * 100.0) as u8,
                description: format!("attempt {}/{max}", attempt + 1),
            });

            sleep(interval).await;
        }

        error!(
            deployment_id = %dep.id,
            max = max,
            "healthcheck: esgotou todas as tentativas"
        );
        Err(anyhow!("healthcheck failed after {max} retries"))
    }
}

/// Mensagem do deploy que falhou porque o container de staging parou.
pub(super) fn stopped_container_message(
    exit_code: Option<i64>,
    quando: &str,
    last_line: Option<&str>,
) -> String {
    let code = exit_code
        .map(|c| format!(" com código {c}"))
        .unwrap_or_default();
    let tail = last_line
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| format!(": {l}"))
        .unwrap_or_default();
    format!("o container saiu{code} {quando}{tail}")
}

#[cfg(test)]
mod stopped_container_tests {
    use super::stopped_container_message;

    #[test]
    fn mensagem_traz_codigo_momento_e_ultima_linha() {
        assert_eq!(
            stopped_container_message(
                Some(1),
                "antes do healthcheck",
                Some("Error: DATABASE_URL não definida")
            ),
            "o container saiu com código 1 antes do healthcheck: Error: DATABASE_URL não definida"
        );
    }

    #[test]
    fn sem_codigo_nem_log_ainda_diz_o_que_houve() {
        assert_eq!(
            stopped_container_message(None, "durante o healthcheck", None),
            "o container saiu durante o healthcheck"
        );
        assert_eq!(
            stopped_container_message(Some(0), "antes do healthcheck", Some("   ")),
            "o container saiu com código 0 antes do healthcheck"
        );
    }
}
