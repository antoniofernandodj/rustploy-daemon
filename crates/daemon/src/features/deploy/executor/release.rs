//! Releases no executor: gravar a release quando o deploy chega a Live e, num deploy que é um
//! "voltar para a vN", preparar a imagem daquela release no lugar do build.
//! Ver `docs/arquivo/plano-releases-ambientes-rotas-volumes.md` §1.

use super::*;
use crate::domain::{ServiceSource, image_ref_is_pullable, pick_image_ref, split_repository_tag};
use crate::store::releases::{self, NewRelease};

impl DeployExecutor {
    /// Grava a release de um deploy que acabou de virar Live. Nunca falha o deploy: o serviço já
    /// está no ar; sem a release, só o "voltar" para este ponto deixa de existir (fica no log).
    pub(super) async fn record_release(&self, dep: &Deployment, svc: &Service) {
        let is_compose = matches!(svc.spec.source, ServiceSource::Compose(_));
        let image = if is_compose {
            String::new()
        } else {
            self.image_for(dep, svc)
        };
        let image_ref = if is_compose {
            None
        } else {
            match self.docker.inner.inspect_image(&image).await {
                Ok(inspected) => pick_image_ref(
                    matches!(svc.spec.source, ServiceSource::Registry { .. }),
                    &image,
                    inspected.id.as_deref(),
                    inspected.repo_digests.as_deref().unwrap_or_default(),
                ),
                Err(error) => {
                    warn!(deployment_id = %dep.id, image = %image, error = %error,
                        "release: não inspecionei a imagem; a release fica sem referência fixa");
                    None
                }
            }
        };
        let project_env = match crate::store::projects::get(&self.db, &svc.spec.project_id).await {
            Ok(Some(project)) => project.env_vars,
            _ => Vec::new(),
        };
        // De onde veio a imagem: uma release deste serviço ("volta da vN") ou de um serviço irmão
        // noutro ambiente ("promovida de loja (homologação) v7").
        let source_release = match releases::deployment_source(&self.db, &dep.id).await {
            Ok(Some(release_id)) => releases::get(&self.db, &release_id).await.ok().flatten(),
            _ => None,
        };
        let (restored_from_number, promoted_from) = match &source_release {
            Some(release) if release.service_id == svc.id => (Some(release.number), None),
            Some(release) => {
                let project_name =
                    match crate::store::services::get(&self.db, &release.service_id).await {
                        Ok(Some(source_service)) => {
                            crate::store::projects::get(&self.db, &source_service.spec.project_id)
                                .await
                                .ok()
                                .flatten()
                                .map(|project| project.name)
                                .unwrap_or_default()
                        }
                        _ => String::new(),
                    };
                (
                    None,
                    Some(
                        format!("{project_name} v{}", release.number)
                            .trim()
                            .to_string(),
                    ),
                )
            }
            None => (None, None),
        };
        let created = releases::create(
            &self.db,
            NewRelease {
                service_id: &svc.id,
                deployment_id: &dep.id,
                image: &image,
                image_ref: image_ref.as_deref(),
                spec: &svc.spec,
                project_env: &project_env,
                restored_from_number,
                promoted_from: promoted_from.as_deref(),
            },
        )
        .await;
        match created {
            Ok(release) => {
                let line = match (&release.promoted_from, release.restored_from_number) {
                    (Some(source), _) => {
                        format!(
                            "--> Release v{} registrada (promovida de {source})",
                            release.number
                        )
                    }
                    (None, Some(origin)) => {
                        format!(
                            "--> Release v{} registrada (volta da v{origin})",
                            release.number
                        )
                    }
                    (None, None) => format!("--> Release v{} registrada", release.number),
                };
                self.log_step(&dep.id, &svc.id, &line).await;
            }
            Err(error) => {
                error!(deployment_id = %dep.id, error = %error, "release: falha ao gravar");
                self.log_step(
                    &dep.id,
                    &svc.id,
                    &format!("--> Aviso: a release deste deploy não foi gravada ({error})"),
                )
                .await;
            }
        }
    }

    /// Deploy que é um "voltar para a vN" (`deployment.from_release_id`), de fonte que não é Compose:
    /// garante a imagem da release neste servidor (baixa pelo digest se preciso) e a marca com o nome
    /// que o staging vai usar. Devolve `Ok(true)` quando preparou (o deploy pula pull e build).
    pub(super) async fn prepare_release_image(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<bool> {
        if matches!(svc.spec.source, ServiceSource::Compose(_)) {
            return Ok(false);
        }
        let Some(release_id) = releases::deployment_source(&self.db, &dep.id).await? else {
            return Ok(false);
        };
        let release = releases::get(&self.db, &release_id)
            .await?
            .ok_or_else(|| anyhow!("a release {release_id} não existe mais"))?;
        let image_ref = release.image_ref.clone().ok_or_else(|| {
            anyhow!(
                "a v{} não tem referência fixa de imagem: não dá para voltar para ela",
                release.number
            )
        })?;
        self.log_step(
            &dep.id,
            &svc.id,
            &format!(
                "--> Voltando para a v{}: imagem {image_ref}",
                release.number
            ),
        )
        .await;

        if self.docker.inner.inspect_image(&image_ref).await.is_err() {
            if !image_ref_is_pullable(&image_ref) {
                return Err(anyhow!(
                    "a imagem da v{} ({image_ref}) não existe mais neste servidor (apagada pela \
                     limpeza do Docker?) e foi construída aqui: não há de onde baixá-la",
                    release.number
                ));
            }
            let credentials = self.registry_credentials_for(&image_ref).await;
            images::pull(
                &self.docker.inner,
                &image_ref,
                &svc.id,
                &dep.id,
                &self.bus,
                &self.db,
                credentials,
            )
            .await?;
        }

        // O staging cria o container a partir de `image_for` (a tag deste deploy, ou a tag do
        // registry): ela passa a apontar para a imagem da release.
        let target = self.image_for(dep, svc);
        let (repo, tag) = split_repository_tag(&target)
            .ok_or_else(|| anyhow!("nome de imagem sem tag: {target}"))?;
        self.docker
            .inner
            .tag_image(
                &image_ref,
                Some(bollard::image::TagImageOptions {
                    repo: repo.as_str(),
                    tag: tag.as_str(),
                }),
            )
            .await
            .map_err(|error| anyhow!("não consegui marcar {image_ref} como {target}: {error}"))?;
        self.log_step(
            &dep.id,
            &svc.id,
            &format!(
                "--> Imagem da v{} pronta como {target} (sem build)",
                release.number
            ),
        )
        .await;
        Ok(true)
    }
}
