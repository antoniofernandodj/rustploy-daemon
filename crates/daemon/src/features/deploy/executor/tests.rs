use super::*;

#[cfg(test)]
mod embedded_registry_image_tests {
    use super::super::is_embedded_registry_image;

    #[test]
    fn reconhece_loopback_com_porta_certa() {
        assert!(is_embedded_registry_image(
            "127.0.0.1:5100/app:v1",
            5100,
            None
        ));
    }

    #[test]
    fn nao_reconhece_porta_errada() {
        assert!(!is_embedded_registry_image(
            "127.0.0.1:9999/app:v1",
            5100,
            None
        ));
    }

    #[test]
    fn reconhece_localhost() {
        assert!(is_embedded_registry_image(
            "localhost:5100/app:v1",
            5100,
            None
        ));
    }

    #[test]
    fn reconhece_dominio_configurado_sem_porta() {
        assert!(is_embedded_registry_image(
            "registry.exemplo.com/app:v1",
            5100,
            Some("registry.exemplo.com")
        ));
    }

    #[test]
    fn nao_reconhece_dominio_com_porta_5100_anexada() {
        // domínio:porta NUNCA é a forma certa (porta só existe em loopback) —
        // garantir que esse caso não bate por acidente com o prefixo do domínio.
        assert!(!is_embedded_registry_image(
            "registry.exemplo.com:5100/app:v1",
            5100,
            Some("registry.exemplo.com")
        ));
    }

    #[test]
    fn imagem_externa_nao_bate() {
        assert!(!is_embedded_registry_image("nginx:latest", 5100, None));
        assert!(!is_embedded_registry_image(
            "ghcr.io/user/app:v1",
            5100,
            Some("registry.exemplo.com")
        ));
    }
}

/// Testa só os dois ramos de `step[PreDeployCheck]` que não tocam Docker
/// (sem job configurado = no-op; job apagado/inexistente = falha) — o ramo
/// de exit code exigiria um `docker compose up` real, fora do escopo de um
/// teste unitário (nenhum outro teste deste crate sobe containers de verdade).
#[cfg(test)]
mod pre_deploy_check_tests {
    use super::*;
    use crate::platform::config::AcmeConfig;
    use crate::{
        engine::DockerClient,
        features::ingress::{IngressController, TlsManager},
        platform::event_bus::EventBus,
        platform::secrets::SecretsManager,
        store,
    };
    use shared::{Healthcheck, ResourceLimits, ServiceSource, ServiceSpec};
    use ulid::Ulid;

    pub(super) async fn test_executor() -> DeployExecutor {
        let dir = std::env::temp_dir().join(format!("rustploy_test_predeploy_{}", Ulid::new()));
        let db = Arc::new(store::connect(&dir).await.unwrap());
        let docker = Arc::new(DockerClient::for_tests());
        let secrets = Arc::new(SecretsManager::new(&dir.join("master.key"), db.clone()).unwrap());
        let tls = Arc::new(
            TlsManager::new(
                dir.join("certs"),
                AcmeConfig {
                    enabled: false,
                    email: None,
                    directory: String::new(),
                },
            )
            .unwrap(),
        );
        DeployExecutor {
            db,
            docker,
            ingress: Arc::new(IngressController::new()),
            bus: Arc::new(EventBus::new()),
            secrets,
            tls,
            db_path: dir,
            drain_secs: 5,
            registry_internal_token: None,
        }
    }

    pub(super) fn spec(pre_deploy_job_ids: Vec<String>) -> ServiceSpec {
        ServiceSpec {
            name: format!("svc-{}", Ulid::new()),
            project_id: "proj-1".into(),
            source: ServiceSource::Registry {
                image: "nginx:latest".into(),
            },
            port: 8080,
            host_port: None,
            domain: None,
            tls_enabled: false,
            env_vars: vec![],
            env_comments: vec![],
            volumes: vec![],
            healthcheck: Healthcheck::default(),
            replicas: 1,
            resources: ResourceLimits::default(),
            run_command: None,
            run_args: vec![],
            db_kind: None,
            domains: vec![],
            pre_deploy_job_id: None,
            pre_deploy_job_ids,
            shared: None,
        }
    }

    #[tokio::test]
    async fn fila_vazia_e_no_op() {
        let executor = test_executor().await;
        let svc = store::services::create(&executor.db, spec(vec![]))
            .await
            .unwrap();
        let mut dep = store::deployments::create(&executor.db, &svc.id, "nginx:latest")
            .await
            .unwrap();
        dep.state = DeployState::PreDeployCheck;

        let next = executor.step(&dep, &svc).await.unwrap();
        assert_eq!(next, DeployState::ResolvingDeps);
    }

    #[tokio::test]
    async fn job_inexistente_falha_o_step() {
        let executor = test_executor().await;
        let svc = store::services::create(&executor.db, spec(vec!["job_nao_existe".into()]))
            .await
            .unwrap();
        let mut dep = store::deployments::create(&executor.db, &svc.id, "nginx:latest")
            .await
            .unwrap();
        dep.state = DeployState::PreDeployCheck;

        let err = executor.step(&dep, &svc).await.unwrap_err();
        assert!(err.to_string().contains("não encontrado"));
    }

    /// Fila com dois checks, ambos apontando pra jobs que não existem: a
    /// falha tem que citar o PRIMEIRO da fila (`job_ausente_1`) — prova que o
    /// loop parou ali e nunca chegou a olhar o segundo item.
    #[tokio::test]
    async fn fila_para_no_primeiro_job_ausente_sem_chegar_no_segundo() {
        let executor = test_executor().await;
        let svc = store::services::create(
            &executor.db,
            spec(vec!["job_ausente_1".into(), "job_ausente_2".into()]),
        )
        .await
        .unwrap();
        let mut dep = store::deployments::create(&executor.db, &svc.id, "nginx:latest")
            .await
            .unwrap();
        dep.state = DeployState::PreDeployCheck;

        let err = executor.step(&dep, &svc).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("job_ausente_1"));
        assert!(!msg.contains("job_ausente_2"));
    }

    /// Retrocompat: specs antigos só têm `pre_deploy_job_id` (singular) —
    /// `pre_deploy_checks()` precisa cair nesse legado quando a fila nova
    /// está vazia (ver `ServiceSpec::pre_deploy_checks`).
    #[tokio::test]
    async fn legado_pre_deploy_job_id_ainda_funciona_via_fallback() {
        let executor = test_executor().await;
        let mut s = spec(vec![]);
        s.pre_deploy_job_id = Some("job_legado_nao_existe".into());
        let svc = store::services::create(&executor.db, s).await.unwrap();
        let mut dep = store::deployments::create(&executor.db, &svc.id, "nginx:latest")
            .await
            .unwrap();
        dep.state = DeployState::PreDeployCheck;

        let err = executor.step(&dep, &svc).await.unwrap_err();
        assert!(err.to_string().contains("job_legado_nao_existe"));
    }
}

/// Regressão do "erro de deploy invisível": a causa de um step falho tem que
/// (a) virar linha(s) de `build_log` — o único canal que a tela de log lê — e
/// (b) existir de verdade para o caso do Dockerfile ausente numa fonte `Git`,
/// que antes só o ramo `Archive` checava. Ver
/// `docs/plano-erro-de-deploy-invisivel.md`.
#[cfg(test)]
mod failure_log_tests {
    use super::pre_deploy_check_tests::{spec, test_executor};
    use super::*;
    use crate::store;
    use shared::{GitSource, ServiceSpec, StateTransition};

    fn git_spec() -> ServiceSpec {
        let mut s = spec(vec![]);
        s.source = ServiceSource::Git(GitSource {
            url: "https://github.com/exemplo/app".into(),
            branch: "main".into(),
            dockerfile_path: "Dockerfile".into(),
            build_context: ".".into(),
            ..GitSource::default()
        });
        s
    }

    // ── failure_log_lines ──────────────────────────────────────────────────

    #[test]
    fn causa_de_uma_linha_vira_um_marco_com_o_estado() {
        let linhas = failure_log_lines(
            "BuildingImage",
            "docker build error: Cannot locate specified Dockerfile: Dockerfile",
        );
        assert_eq!(linhas.len(), 1);
        assert_eq!(
            linhas[0],
            "==> Erro em [BuildingImage]: docker build error: \
             Cannot locate specified Dockerfile: Dockerfile"
        );
    }

    /// Erro de `docker build` costuma ser multi-linha: cada linha vira um
    /// registro próprio (o renderizador do cliente trata registro = linha), e
    /// as continuações vêm indentadas sob o marco.
    #[test]
    fn causa_multilinha_vira_varios_registros_indentados() {
        let linhas = failure_log_lines("Staging", "falhou ao subir\n  porta 8080 ocupada\n\n");
        assert_eq!(
            linhas,
            vec![
                "==> Erro em [Staging]: falhou ao subir".to_string(),
                "      porta 8080 ocupada".to_string(),
            ]
        );
    }

    #[test]
    fn causa_vazia_ainda_produz_uma_linha() {
        let linhas = failure_log_lines("Promoting", "   \n  ");
        assert_eq!(linhas.len(), 1);
        assert!(linhas[0].contains("falha sem mensagem"));
    }

    // ── missing_dockerfile_msg ─────────────────────────────────────────────

    /// O erro mais comum não é "não criei o Dockerfile", é "criei mas não fiz
    /// push" — por isso a mensagem do ramo Git cita o branch clonado.
    #[test]
    fn mensagem_do_git_cita_o_branch() {
        let msg = missing_dockerfile_msg(
            &ServiceSource::Git(GitSource {
                branch: "producao".into(),
                build_context: ".".into(),
                ..GitSource::default()
            }),
            "Dockerfile",
        );
        assert!(msg.contains("repositório"));
        assert!(msg.contains("producao"));
        // `build_context` "." não polui a mensagem.
        assert!(!msg.contains("build context"));
    }

    #[test]
    fn mensagem_cita_o_build_context_quando_ele_nao_e_a_raiz() {
        let msg = missing_dockerfile_msg(
            &ServiceSource::Archive(shared::ArchiveSource {
                build_context: "apps/web".into(),
                ..Default::default()
            }),
            "Dockerfile",
        );
        assert!(msg.contains("zip"));
        assert!(msg.contains("build context: apps/web"));
    }

    // ── rollback_cause ─────────────────────────────────────────────────────

    #[test]
    fn causa_do_rollback_vem_da_transicao_que_entrou_em_rollingback() {
        let mut dep = Deployment {
            id: "dep_1".into(),
            service_id: "svc_1".into(),
            image: "nginx".into(),
            state: DeployState::RollingBack,
            states_log: vec![],
            started_at: Utc::now(),
            finished_at: None,
        };
        dep.states_log.push(StateTransition {
            from: DeployState::Pending,
            to: DeployState::BuildingImage,
            at: Utc::now(),
            message: None,
        });
        dep.states_log.push(StateTransition {
            from: DeployState::BuildingImage,
            to: DeployState::RollingBack,
            at: Utc::now(),
            message: Some("docker build error: sem Dockerfile\ndetalhe ignorado".into()),
        });

        // Só a primeira linha, sem a continuação — este texto vira chip de
        // status na UI, não log.
        assert_eq!(rollback_cause(&dep), "docker build error: sem Dockerfile");
    }

    #[test]
    fn causa_do_rollback_cai_no_texto_generico_quando_nao_ha_mensagem() {
        let dep = Deployment {
            id: "dep_1".into(),
            service_id: "svc_1".into(),
            image: "nginx".into(),
            state: DeployState::RollingBack,
            states_log: vec![],
            started_at: Utc::now(),
            finished_at: None,
        };
        assert_eq!(rollback_cause(&dep), "deploy failed");
    }

    #[test]
    fn causa_longa_e_truncada_por_caractere() {
        let longa = "ç".repeat(400);
        let cortada = truncate_chars(&longa, 160);
        // Truncar por índice de BYTE no meio de um multibyte entraria em pânico.
        assert_eq!(cortada.chars().count(), 160);
        assert!(cortada.ends_with('…'));
    }

    // ── o step de build falha ANTES do Docker quando falta o Dockerfile ─────

    /// Fonte `Git` sem Dockerfile no contexto: o step tem que falhar sozinho,
    /// com mensagem acionável, sem nunca chamar o `images::build` (este teste
    /// roda sem Docker, e o diretório de clone nem existe).
    #[tokio::test]
    async fn step_building_image_falha_com_mensagem_util_sem_dockerfile() {
        let executor = test_executor().await;
        let svc = store::services::create(&executor.db, git_spec())
            .await
            .unwrap();
        let mut dep = store::deployments::create(&executor.db, &svc.id, "rp_app:1")
            .await
            .unwrap();
        dep.state = DeployState::BuildingImage;

        let err = executor.step(&dep, &svc).await.unwrap_err().to_string();
        assert!(
            err.contains("Dockerfile não encontrado no repositório"),
            "{err}"
        );
        assert!(err.contains("main"), "{err}");
    }

    /// A ponta que faltava: a causa persistida no `build_log`, que é de onde a
    /// tela de log do serviço lê. Antes desta correção nada escrevia o erro ali
    /// e o log terminava num "==> Deploy falhou" sem motivo.
    #[tokio::test]
    async fn causa_do_step_falho_e_persistida_no_build_log() {
        let executor = test_executor().await;
        let svc = store::services::create(&executor.db, git_spec())
            .await
            .unwrap();
        let dep = store::deployments::create(&executor.db, &svc.id, "rp_app:1")
            .await
            .unwrap();

        let erro = executor
            .step(
                &Deployment {
                    state: DeployState::BuildingImage,
                    ..dep.clone()
                },
                &svc,
            )
            .await
            .unwrap_err();

        for linha in failure_log_lines(DeployState::BuildingImage.label(), &erro.to_string()) {
            executor.log_step(&dep.id, &svc.id, &linha).await;
        }

        let log = store::build_logs::get_for_deployment(&executor.db, &dep.id)
            .await
            .unwrap();
        assert!(!log.is_empty());
        assert!(
            log[0].line.starts_with("==> Erro em [BuildingImage]:"),
            "{}",
            log[0].line
        );
        assert!(
            log.iter()
                .any(|l| l.line.contains("Dockerfile não encontrado no repositório")),
            "log: {:?}",
            log.iter().map(|l| &l.line).collect::<Vec<_>>()
        );
    }
}
