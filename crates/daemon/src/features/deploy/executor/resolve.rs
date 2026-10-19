//! Estágios iniciais: rede do projeto, pré-deploy checks e escolha do caminho pela fonte do serviço.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_pending(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        info!(
            deployment_id = %dep.id,
            project_id = %svc.spec.project_id,
            "step[Pending]: garantindo rede Docker do projeto"
        );
        self.log_step(&dep.id, &svc.id, "==> Iniciando deploy")
            .await;
        let net = self.ensure_network(&svc.spec.project_id).await?;
        info!(
            deployment_id = %dep.id,
            network = %net,
            "step[Pending]: rede pronta"
        );
        Ok(DeployState::PreDeployCheck)
    }

    pub(super) async fn step_pre_deploy_check(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        // Fila inteira roda dentro deste ÚNICO step (sem sub-estado por
        // índice): o loop de `execute()` não impõe timeout por step, e
        // manter a fila num só `PreDeployCheck` evita precisar persistir
        // "em qual item da fila estamos" (sem mudança de schema/wire).
        let checks = svc.spec.pre_deploy_checks();
        if checks.is_empty() {
            return Ok(DeployState::ResolvingDeps);
        }
        let total = checks.len();

        for (idx, job_id) in checks.iter().enumerate() {
            let job = crate::db::job::get(&self.db, job_id)
                .await?
                .ok_or_else(|| anyhow!("job de pré-deploy check não encontrado: {job_id}"))?;

            info!(
                deployment_id = %dep.id,
                job_id = %job.id,
                job_name = %job.name,
                step = idx + 1,
                total,
                "step[PreDeployCheck]: rodando job de pré-deploy"
            );
            self.log_step(
                &dep.id,
                &svc.id,
                &format!("==> Pré-deploy check {}/{}: {}", idx + 1, total, job.name),
            )
            .await;

            let run = crate::db::job_run::create(&self.db, &job.id).await?;
            self.bus.publish(Event::JobRunStateChanged {
                job_id: job.id.clone(),
                job_run_id: run.id.clone(),
                running: true,
                success: None,
            });

            let runner = crate::features::jobs::runner::JobRunner {
                db: self.db.clone(),
                docker: self.docker.clone(),
                bus: self.bus.clone(),
                secrets: self.secrets.clone(),
                db_path: self.db_path.clone(),
                registry_internal_token: self.registry_internal_token.clone(),
            };
            let result = runner
                .run_inner_mirrored(&job, &run.id, Some((dep.id.clone(), svc.id.clone())), None)
                .await;

            let (exit_code, success) = match &result {
                Ok(code) => (*code, *code == 0),
                Err(e) => {
                    warn!(
                        deployment_id = %dep.id,
                        job_id = %job.id,
                        error = %e,
                        "step[PreDeployCheck]: falha ao executar job"
                    );
                    let _ = crate::db::job_run::finish(&self.db, &run.id, -1).await;
                    (-1, false)
                }
            };

            self.bus.publish(Event::JobRunStateChanged {
                job_id: job.id.clone(),
                job_run_id: run.id.clone(),
                running: false,
                success: Some(success),
            });

            if !success {
                // Primeira falha interrompe a fila inteira — os checks
                // seguintes não rodam.
                return Err(anyhow!(
                    "pré-deploy check {}/{} ({}) falhou (exit code {exit_code})",
                    idx + 1,
                    total,
                    job.name
                ));
            }
            self.log_step(
                &dep.id,
                &svc.id,
                &format!("--> Pré-deploy check {}/{} OK", idx + 1, total),
            )
            .await;
        }

        Ok(DeployState::ResolvingDeps)
    }

    pub(super) async fn step_resolving_deps(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let next = match &svc.spec.source {
            ServiceSource::Registry { image } => {
                info!(
                    deployment_id = %dep.id,
                    image = %image,
                    "step[ResolvingDeps]: fonte é Registry → irá para PullingImage"
                );
                self.log_step(&dep.id, &svc.id, &format!("--> Pulling image: {image}"))
                    .await;
                DeployState::PullingImage
            }
            ServiceSource::Git(g) => {
                info!(
                    deployment_id = %dep.id,
                    url = %g.url,
                    branch = %g.branch,
                    "step[ResolvingDeps]: fonte é Git → irá para CloningRepo"
                );
                self.log_step(
                    &dep.id,
                    &svc.id,
                    &format!("--> Clonando repositório: {} ({})", g.url, g.branch),
                )
                .await;
                DeployState::CloningRepo
            }
            ServiceSource::Archive(a) => {
                info!(
                    deployment_id = %dep.id,
                    archive_id = %a.archive_id,
                    "step[ResolvingDeps]: fonte é Archive → irá para BuildingImage"
                );
                self.log_step(&dep.id, &svc.id, "--> Preparando zip enviado")
                    .await;
                DeployState::BuildingImage
            }
            ServiceSource::Compose(c) => {
                info!(
                    deployment_id = %dep.id,
                    compose_file = %c.content,
                    "step[ResolvingDeps]: fonte é Compose → irá para ComposingUp"
                );
                self.log_step(&dep.id, &svc.id, "--> Executando docker compose up")
                    .await;
                DeployState::ComposingUp
            }
        };
        Ok(next)
    }
}
