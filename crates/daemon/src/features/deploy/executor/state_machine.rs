//! Máquina de estados do deploy: o laço `execute` e o despacho de cada `DeployState` para o seu estágio.

use super::*;

impl DeployExecutor {
    pub async fn run(self: Arc<Self>, deployment_id: String) {
        info!(deployment_id = %deployment_id, "executor: iniciando");
        if let Err(e) = self.execute(&deployment_id).await {
            error!(deployment_id = %deployment_id, error = %e, "executor: falha fatal no deploy");
            // Publica o erro para o cliente ver (erros no próprio loop de controle,
            // ex.: falha ao ler do banco — distintos das falhas de step que já fazem rollback)
            self.bus.publish(Event::Error {
                code: "ExecutorFatal".into(),
                message: format!("Falha crítica no deploy {}: {e}", {
                    let s = deployment_id
                        .find('_')
                        .map(|i| &deployment_id[i + 1..])
                        .unwrap_or(&deployment_id);
                    &s[..8.min(s.len())]
                }),
            });
        }
        info!(deployment_id = %deployment_id, "executor: encerrado");
    }

    /// Laço da máquina de estados: chama `step` até um estado terminal,
    /// persistindo cada transição e, se um passo falha, gravando a causa real
    /// no log de build (o único canal que a tela de log lê).
    async fn execute(&self, deployment_id: &str) -> Result<()> {
        loop {
            let deployment = self.load_deployment(deployment_id).await?;
            info!(
                deployment_id = %deployment_id,
                state = deployment.state.label(),
                "executor: estado atual"
            );

            if deployment.state.is_terminal() {
                info!(
                    deployment_id = %deployment_id,
                    state = deployment.state.label(),
                    "executor: estado terminal, saindo do loop"
                );
                break;
            }

            let service = self
                .load_service(&deployment.service_id)
                .await?
                .ok_or_else(|| anyhow!("service not found: {}", deployment.service_id))?;

            info!(
                deployment_id = %deployment_id,
                service_id = %service.id,
                service_name = %service.spec.name,
                state = deployment.state.label(),
                "executor: executando step"
            );

            let result = self.step(&deployment, &service).await;

            match result {
                Ok(next_state) => {
                    info!(
                        deployment_id = %deployment_id,
                        from = deployment.state.label(),
                        to = next_state.label(),
                        "executor: transição de estado"
                    );
                    self.transition(deployment_id, &deployment.state, next_state, None)
                        .await?;
                }
                Err(e) => {
                    warn!(
                        deployment_id = %deployment_id,
                        state = deployment.state.label(),
                        error = %e,
                        "executor: step falhou, iniciando rollback"
                    );
                    // A causa REAL da falha vai para o `build_log` — o único
                    // canal que a tela de log do serviço lê. Ela já era gravada
                    // em outros três (o `warn!` acima, o `states_log` do
                    // deployment e o `Event::DeployStateChanged` do SSE), e
                    // nenhum deles chega ao painel de log: o usuário via o
                    // deploy morrer num "==> Deploy falhou" mudo enquanto o
                    // motivo ("Cannot locate specified Dockerfile", healthcheck
                    // que não passou, pull negado…) existia no banco.
                    //
                    // ANTES do `transition` de propósito: assim a linha do
                    // motivo cai imediatamente acima do
                    // "==> Deploy falhou — iniciando rollback" que o
                    // `step[RollingBack]` grava em seguida, e o log se lê na
                    // ordem em que as coisas aconteceram.
                    for line in failure_log_lines(deployment.state.label(), &e.to_string()) {
                        self.log_step(deployment_id, &service.id, &line).await;
                    }
                    self.transition(
                        deployment_id,
                        &deployment.state,
                        DeployState::RollingBack,
                        Some(e.to_string()),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// Executa o trabalho do estado atual do deployment e devolve o próximo.
    pub(super) async fn step(&self, dep: &Deployment, svc: &Service) -> Result<DeployState> {
        match &dep.state {
            DeployState::Pending => self.step_pending(dep, svc).await,
            DeployState::PreDeployCheck => self.step_pre_deploy_check(dep, svc).await,
            DeployState::ResolvingDeps => self.step_resolving_deps(dep, svc).await,
            DeployState::PullingImage => self.step_pulling_image(dep, svc).await,
            DeployState::CloningRepo => self.step_cloning_repo(dep, svc).await,
            DeployState::BuildingImage => self.step_building_image(dep, svc).await,
            DeployState::Staging => self.step_staging(dep, svc).await,
            DeployState::HealthcheckPolling => self.step_healthcheck_polling(dep, svc).await,
            DeployState::SwappingIn => self.step_swapping_in(dep, svc).await,
            DeployState::Draining => self.step_draining(dep, svc).await,
            DeployState::Promoting => self.step_promoting(dep, svc).await,
            DeployState::RollingBack => self.step_rolling_back(dep, svc).await,
            DeployState::ComposingUp => self.step_composing_up(dep, svc).await,
            other => Err(anyhow!("unhandled state: {:?}", other)),
        }
    }
}

/// Quebra a causa de um step falho nas linhas que vão para o `build_log`.
///
/// Multi-linha vira VÁRIAS entradas, e não um registro só com `\n` dentro: o
/// renderizador de log do cliente (`format/service_detail.luau`) trata cada
/// registro como uma linha, então um `\n` embutido sairia espremido numa linha
/// só. Erro de `docker build` é multi-linha com frequência.
///
/// A primeira linha leva o marco `==>` (mesma convenção do resto do log) e cita
/// o estado em que o deploy quebrou — que hoje também se perdia: o log dizia
/// que falhou, nunca *em que etapa*. As continuações entram indentadas, para o
/// olho separar uma causa de várias linhas de vários passos seguidos.
pub(super) fn failure_log_lines(state_label: &str, err: &str) -> Vec<String> {
    let mut linhas = err
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty());

    let Some(primeira) = linhas.next() else {
        // `anyhow!("")` é improvável, mas uma linha dizendo que não há mensagem
        // ainda é melhor que o log terminar mudo — que é o bug todo.
        return vec![format!("==> Erro em [{state_label}]: falha sem mensagem")];
    };

    let mut out = vec![format!("==> Erro em [{state_label}]: {primeira}")];
    out.extend(linhas.map(|l| format!("    {l}")));
    out
}
