//! Estágios de obtenção da imagem: pull do registry, clone do Git e build.

use super::*;

impl DeployExecutor {
    pub(super) async fn step_pulling_image(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let image = self.image_for(dep, svc);
        info!(
            deployment_id = %dep.id,
            image = %image,
            "step[PullingImage]: iniciando pull"
        );
        let creds = self.registry_credentials_for(&image).await;
        images::pull(
            &self.docker.inner,
            &image,
            &svc.id,
            &dep.id,
            &self.bus,
            &self.db,
            creds,
        )
        .await?;
        self.log_step(&dep.id, &svc.id, &format!("--> Pull concluído: {image}"))
            .await;
        info!(
            deployment_id = %dep.id,
            image = %image,
            "step[PullingImage]: pull concluído"
        );
        Ok(DeployState::Staging)
    }

    pub(super) async fn step_cloning_repo(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let ServiceSource::Git(git) = &svc.spec.source else {
            return Err(anyhow!("expected Git source"));
        };
        info!(
            deployment_id = %dep.id,
            url = %git.url,
            branch = %git.branch,
            "step[CloningRepo]: resolvendo credenciais"
        );
        let (token, clone_username) = crate::features::deploy::git::resolve_clone_credentials(
            &self.db,
            &self.secrets,
            git.provider_id.as_deref(),
            git.credentials.as_deref(),
            git.username.as_deref(),
            &svc.spec.project_id,
        )
        .await;

        let dir = self.clone_dir(&dep.id);
        let bus = self.bus.clone();
        let sid = svc.id.clone();
        let did = dep.id.clone();

        info!(
            deployment_id = %dep.id,
            dir = %dir.display(),
            git_url = &git.url,
            "step[CloningRepo]: clonando para diretório"
        );

        crate::features::deploy::git::clone(
            crate::features::deploy::git::CloneOptions {
                url: &git.url,
                branch: &git.branch,
                token: token.as_deref(),
                username: clone_username.as_deref(),
                dir: &dir,
            },
            |p| {
                bus.publish(Event::DeployProgress {
                    deployment_id: did.clone(),
                    service_id: sid.clone(),
                    phase: "CloningRepo".into(),
                    percent: p.percent,
                    description: p.description,
                });
            },
        )
        .await?;

        self.log_step(&dep.id, &svc.id, "--> Clone concluído").await;
        info!(deployment_id = %dep.id, "step[CloningRepo]: clone concluído");
        Ok(DeployState::BuildingImage)
    }

    pub(super) async fn step_building_image(
        &self,
        dep: &Deployment,
        svc: &Service,
    ) -> Result<DeployState> {
        let (context, dockerfile_path) = match &svc.spec.source {
            ServiceSource::Git(git) => {
                let clone_dir = self.clone_dir(&dep.id);
                (
                    clone_dir.join(&git.build_context),
                    git.dockerfile_path.clone(),
                )
            }
            ServiceSource::Archive(archive) => {
                let src = self.archive_dir(&svc.id, &archive.archive_id);
                let dst = self.clone_dir(&dep.id);
                if dst.exists() {
                    let _ = std::fs::remove_dir_all(&dst);
                }
                copy_dir_all(&src, &dst)?;
                (
                    dst.join(&archive.build_context),
                    archive.dockerfile_path.clone(),
                )
            }
            _ => return Err(anyhow!("expected Git or Archive source")),
        };

        // Checagem ANTES de entregar ao Docker, para os dois ramos. Era
        // só do `Archive`: uma fonte `Git` sem Dockerfile commitado ia
        // direto para o `images::build` e voltava um lacônico
        // "docker build error: Cannot locate specified Dockerfile" —
        // que, até a correção acima, nem chegava ao log do usuário.
        //
        // O caminho é relativo ao CONTEXTO, não à raiz do clone/zip: é
        // exatamente o que `create_tar_gz` usa como nome dentro do tar
        // que manda pro Docker. A checagem antiga do `Archive` olhava a
        // raiz, então errava quando o `build_context` não era ".".
        if !context.join(&dockerfile_path).is_file() {
            return Err(anyhow!(
                "{}",
                missing_dockerfile_msg(&svc.spec.source, &dockerfile_path)
            ));
        }

        let tag = format!("rp_{}:{}", svc.spec.safe_name(), self.short(&dep.id));
        info!(
            deployment_id = %dep.id,
            tag = %tag,
            dockerfile = %dockerfile_path,
            context = %context.display(),
            "step[BuildingImage]: iniciando build Docker"
        );
        self.log_step(
            &dep.id,
            &svc.id,
            &format!("--> Build da imagem: {} ({})", tag, dockerfile_path),
        )
        .await;
        images::build(
            &self.docker.inner,
            &self.db,
            &context,
            &dockerfile_path,
            &tag,
            &svc.id,
            &dep.id,
            &self.bus,
        )
        .await?;
        crate::features::maintenance::service_warm::record(&self.db, svc, &tag).await;
        self.log_step(&dep.id, &svc.id, "--> Build concluído").await;
        info!(
            deployment_id = %dep.id,
            tag = %tag,
            "step[BuildingImage]: build concluído"
        );
        Ok(DeployState::Staging)
    }
}

pub(super) fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Mensagem de "Dockerfile não encontrado", especializada por fonte.
///
/// No caso `Git` o erro mais comum não é "esqueci de criar o Dockerfile", é
/// "criei mas não commitei/não fiz push" — por isso a mensagem cita o branch
/// clonado: aponta para essa hipótese sem precisar afirmá-la. O `build_context`
/// só aparece quando não é `.`, porque é ele que muda o significado do caminho
/// (o Dockerfile é procurado DENTRO do contexto).
pub(super) fn missing_dockerfile_msg(source: &ServiceSource, dockerfile_path: &str) -> String {
    let contexto = |ctx: &str| {
        if ctx == "." || ctx.is_empty() {
            String::new()
        } else {
            format!(" (build context: {ctx})")
        }
    };

    match source {
        ServiceSource::Git(git) => format!(
            "Dockerfile não encontrado no repositório: {}{} — branch {}. \
             Confira se ele foi commitado e enviado, e se o caminho configurado \
             no serviço está certo.",
            dockerfile_path,
            contexto(&git.build_context),
            git.branch,
        ),
        ServiceSource::Archive(archive) => format!(
            "Dockerfile não encontrado no zip: {}{}",
            dockerfile_path,
            contexto(&archive.build_context),
        ),
        _ => format!("Dockerfile não encontrado: {dockerfile_path}"),
    }
}
