//! Wizard "Novo serviço" server-side: catálogos (`WizardCatalog`) e criação
//! (`WizardCreate`). A lógica de montagem do `ServiceSpec` vive em
//! `crate::features::wizard` (que tem acesso aos blueprints de `crate::features::templates`); aqui
//! só ligamos aos handlers existentes.

use crate::bootstrap::state::AppState;
use crate::features::wizard;
use shared::Response as RpResponse;
use shared::WizardCreateReq;

/// Catálogos de bancos/brokers/templates prontos como JSON para o contexto do
/// cliente (`new_service_databases`/`new_service_brokers`/`new_service_templates`). `search` filtra os templates.
pub async fn catalog(search: String) -> RpResponse {
    RpResponse::WizardCatalog {
        dbs: wizard::db_rows_json(),
        brokers: wizard::broker_rows_json(),
        templates: wizard::templates_catalog_json(&search),
    }
}

/// Monta o `ServiceSpec` a partir dos campos coletados pelo wizard e cria o
/// serviço — reaproveitando o handler `service_create`.
///
/// **Banco e broker novos já nascem com as credenciais em secret de serviço** (decisão da R5 do
/// plano `docs/plano-backup-estado-em-git.md`): depois de criado, cada env de senha vira
/// `Secret(<chave>)` e o valor passa a viver cifrado em `ServiceInfra.secrets`. Serviços que já
/// existem não são tocados: só mudam por ação manual.
pub async fn create(state: AppState, req: WizardCreateReq) -> RpResponse {
    let spec = match wizard::build_spec(&req) {
        Ok(spec) => spec,
        Err(e) => return RpResponse::err("WizardError", e),
    };
    let resp = crate::features::services::create::handle(state.services_ctx(), spec).await;
    if matches!(req.kind.as_str(), "database" | "broker")
        && let RpResponse::Service(svc) = &resp
    {
        return match secure_credentials(&state, svc).await {
            Ok(Some(fresh)) => RpResponse::Service(Box::new(fresh)),
            Ok(None) => resp,
            Err(e) => {
                // O serviço já foi criado; a senha ficou como env em claro. Não derruba a criação,
                // mas avisa: a pessoa pode mover à mão e a auditoria do git vai apontá-la.
                tracing::warn!(service_id = %svc.id, error = %e, "wizard: não consegui mover a senha do banco para secret");
                resp
            }
        };
    }
    resp
}

/// Move as envs de credencial (`Plain` não vazias) do serviço recém-criado para secrets do serviço.
/// Devolve o serviço relido, ou `None` se não havia nada a mover.
async fn secure_credentials(
    state: &AppState,
    svc: &shared::Service,
) -> anyhow::Result<Option<shared::Service>> {
    let keys: Vec<String> = svc
        .spec
        .env_vars
        .iter()
        .filter(|v| match &v.value {
            shared::EnvVarValue::Plain(value) => {
                !value.is_empty() && crate::store::infra::audit::name_looks_sensitive(&v.key)
            }
            shared::EnvVarValue::Secret(_) => false,
        })
        .map(|v| v.key.clone())
        .collect();
    if keys.is_empty() {
        return Ok(None);
    }
    for key in &keys {
        state
            .secrets
            .move_secret(
                &svc.spec.project_id,
                Some(&svc.id),
                key,
                shared::SecretMoveOp::EnvToSecret,
            )
            .await?;
    }
    crate::store::services::get(&state.db, &svc.id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::EnvVarValue;

    async fn estado() -> AppState {
        let tmp = std::env::temp_dir().join(format!("rustploy-wizard-test-{}", ulid::Ulid::new()));
        let config = crate::platform::config::RustployConfig::default();
        let db = std::sync::Arc::new(crate::store::connect(&tmp).await.unwrap());
        let secrets =
            std::sync::Arc::new(crate::platform::secrets::SecretsManager::new(db.clone()).unwrap());
        let tls = std::sync::Arc::new(
            crate::features::ingress::TlsManager::new(
                tmp.join("certs"),
                config.ingress.acme.clone(),
            )
            .unwrap(),
        );
        AppState::new(
            db,
            std::sync::Arc::new(crate::engine::DockerClient::for_tests()),
            std::sync::Arc::new(crate::features::ingress::IngressController::new()),
            std::sync::Arc::new(crate::platform::event_bus::EventBus::new()),
            secrets,
            tls,
            tmp.join("db"),
            tmp.join("backup"),
            30,
            config.api,
            &config.update,
            None,
            None,
        )
    }

    fn req(kind: &str, id: &str, project_id: &str) -> WizardCreateReq {
        WizardCreateReq {
            kind: kind.into(),
            id: id.into(),
            project_id: project_id.into(),
            name: "meu-banco".into(),
            app_name: String::new(),
            db_name: "app".into(),
            user: "app".into(),
            password: "senha-do-banco-123".into(),
            root_password: "senha-root-456".into(),
            image: String::new(),
            use_replica: false,
            template_values: vec![],
            expose_external: false,
        }
    }

    #[tokio::test]
    async fn banco_novo_nasce_com_a_senha_em_secret_de_servico() {
        let state = estado().await;
        let project = crate::store::projects::create(&state.db, "p".into(), None)
            .await
            .unwrap();
        let RpResponse::Service(svc) =
            create(state.clone(), req("database", "mysql", &project.id)).await
        else {
            panic!("devia criar o serviço")
        };
        let value = |key: &str| {
            svc.spec
                .env_vars
                .iter()
                .find(|v| v.key == key)
                .map(|v| v.value.clone())
        };
        // senhas viraram referência; o que não é segredo continua legível
        assert_eq!(
            value("MYSQL_PASSWORD"),
            Some(EnvVarValue::Secret("MYSQL_PASSWORD".into()))
        );
        assert_eq!(
            value("MYSQL_ROOT_PASSWORD"),
            Some(EnvVarValue::Secret("MYSQL_ROOT_PASSWORD".into()))
        );
        assert_eq!(value("MYSQL_USER"), Some(EnvVarValue::Plain("app".into())));
        // os valores vivem cifrados no escopo do serviço, e a resolução os devolve
        assert_eq!(
            state
                .secrets
                .get_scoped(&project.id, Some(&svc.id), "MYSQL_PASSWORD")
                .await
                .unwrap(),
            "senha-do-banco-123"
        );
        let resolved: std::collections::HashMap<_, _> =
            crate::features::deploy::env_resolve::resolve(&state.db, &state.secrets, &svc)
                .await
                .unwrap()
                .into_iter()
                .collect();
        assert_eq!(resolved["MYSQL_PASSWORD"], "senha-do-banco-123");
        // e nenhum arquivo do YAML contém a senha em claro
        let tree = state.db.infra().tree();
        let tudo: String = tree
            .list()
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&tree.read(r).unwrap().unwrap()).into_owned())
            .collect();
        assert!(!tudo.contains("senha-do-banco-123") && !tudo.contains("senha-root-456"));
        // a auditoria de git não acha a senha do banco novo
        let achados = crate::store::infra::audit::plaintext_findings(&state.db.infra().snapshot());
        assert!(
            achados.iter().all(|a| !a.what.contains("PASSWORD")),
            "{achados:?}"
        );
    }

    #[tokio::test]
    async fn aplicacao_nao_tem_suas_envs_mexidas() {
        let state = estado().await;
        let project = crate::store::projects::create(&state.db, "p".into(), None)
            .await
            .unwrap();
        let RpResponse::Service(svc) =
            create(state.clone(), req("application", "", &project.id)).await
        else {
            panic!("devia criar o serviço")
        };
        assert!(svc.spec.env_vars.is_empty());
        assert!(
            state
                .secrets
                .list_names_scoped(&project.id, Some(&svc.id))
                .await
                .unwrap()
                .is_empty()
        );
    }
}
