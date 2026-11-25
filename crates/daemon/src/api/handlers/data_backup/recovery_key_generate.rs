//! `Command::RecoveryKeyGenerate`: gera a chave de recuperação; a privada sai só nesta resposta, dentro do
//! arquivo de recuperação (`.rpkit`) com o repositório da infra, a branch e o token.
//! Planos: `docs/arquivo/plano-chave-de-recuperacao.md` e `docs/arquivo/plano-arquivo-de-recuperacao.md`.

use base64::Engine as _;

use crate::bootstrap::state::AppState;
use crate::domain::GitAuthMode;
use crate::features::data_backup::recovery_kit::RecoveryKit;
use crate::protocol::Response;

/// O que vai no arquivo além da chave, já conferido.
struct InfraAccess {
    remote: String,
    branch: String,
    token: String,
    warning: Option<String>,
}

/// Remoto local (`file://` ou caminho absoluto): não tem credencial.
fn is_local(remote: &str) -> bool {
    remote.starts_with("file://") || remote.starts_with('/')
}

/// Confere repositório, branch e conta e tira o token. Roda **antes** de gerar a chave: um erro aqui
/// não pode trocar a chave do servidor.
async fn infra_access(
    state: &AppState,
    infra_remote: String,
    infra_branch: Option<String>,
    provider_id: Option<String>,
) -> Result<InfraAccess, Response> {
    let remote = infra_remote.trim().to_string();
    let branch = infra_branch.unwrap_or_default().trim().to_string();
    if remote.is_empty() {
        return Err(Response::err(
            "InvalidInput",
            "informe o repositório da infra: o arquivo de recuperação leva o endereço, a branch e o \
             token junto com a chave (atualize o app se esta tela não pede esses campos)",
        ));
    }
    if is_local(&remote) {
        return Ok(InfraAccess {
            remote,
            branch,
            token: String::new(),
            warning: None,
        });
    }
    if !remote.starts_with("https://") {
        return Err(Response::err(
            "InvalidInput",
            "o repositório da infra precisa ser https://… (endereços SSH, git@…, não são aceitos)",
        ));
    }
    let Some(provider_id) = provider_id.filter(|id| !id.trim().is_empty()) else {
        return Err(Response::err(
            "InvalidInput",
            "escolha a conta Git cujo token vai no arquivo de recuperação",
        ));
    };
    let provider = match crate::store::git_providers::get(&state.db, &provider_id).await {
        Ok(Some(provider)) => provider,
        Ok(None) => return Err(Response::err("NotFound", "conta Git não encontrada")),
        Err(e) => return Err(Response::err("StoreError", format!("{e:#}"))),
    };
    // Só entrega o token se o repositório for do host da conta: senão quem escolhe a URL levaria, no
    // arquivo, o token de outro servidor.
    let token = match crate::features::git_providers::usable_token_for_url(
        &state.secrets,
        &provider,
        &remote,
    ) {
        Ok(token) => token,
        Err(e) => return Err(Response::err("InvalidInput", format!("{e:#}"))),
    };
    let warning = (GitAuthMode::from_str(&provider.auth_mode) == Some(GitAuthMode::OAuth)).then(|| {
        format!(
            "A conta '{}' entra por OAuth: o token dela pode expirar antes de um desastre. Para o \
             arquivo de recuperação, prefira uma conta com token pessoal (PAT) só de leitura do \
             repositório da infra.",
            provider.name
        )
    });
    Ok(InfraAccess {
        remote,
        branch,
        token,
        warning,
    })
}

pub async fn handle(
    state: &AppState,
    replace: bool,
    infra_remote: String,
    infra_branch: Option<String>,
    provider_id: Option<String>,
) -> Response {
    let access = match infra_access(state, infra_remote, infra_branch, provider_id).await {
        Ok(access) => access,
        Err(response) => return response,
    };
    match state.secrets.generate_recovery(replace) {
        Ok((public_key, private_key)) => {
            // Os segredos que já existem estão cifrados só para a chave do servidor (ou para a chave de
            // recuperação ANTERIOR): recifra-os para incluir a nova, senão ela só abriria o que for
            // gravado daqui em diante. A chave já está gravada e a privada já existe, então uma falha aqui
            // não desfaz nada: vai para o log (e o ciclo normal recifra o que for regravado).
            match crate::store::infra::rekey::rekey_all(state.db.infra(), &state.secrets) {
                Ok(n) => tracing::info!(valores = n, "chave de recuperação: segredos recifrados"),
                Err(e) => {
                    tracing::warn!("chave de recuperação: não consegui recifrar os segredos: {e:#}")
                }
            }
            let host = server_name();
            let kit = RecoveryKit {
                infra_remote: access.remote,
                infra_branch: access.branch,
                infra_token: access.token,
                private_key: private_key.clone(),
                public_key: public_key.clone(),
                server: host.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            // A chave já foi gravada: se o arquivo não puder ser montado (não acontece com campos deste
            // tamanho), a privada ainda volta na resposta para a pessoa não ficar sem ela.
            // Relê o que vai sair antes de devolver: o arquivo é a única cópia da chave, e um defeito
            // no escritor tem de aparecer aqui, não no dia do desastre.
            let encoded = kit
                .encode()
                .and_then(|bytes| RecoveryKit::decode(&bytes).map(|read| (bytes, read)));
            let (file_base64, warning) = match encoded {
                Ok((bytes, read)) if read == kit => (
                    base64::prelude::BASE64_STANDARD.encode(bytes),
                    access.warning,
                ),
                Ok(_) => (
                    String::new(),
                    Some(
                        "o arquivo de recuperação não releu igual (bug): use a chave mostrada"
                            .into(),
                    ),
                ),
                Err(e) => (String::new(), Some(format!("{e:#}"))),
            };
            Response::RecoveryKeyGenerated {
                suggested_filename: format!(
                    "rustploy-recuperacao-{host}-{}.rpkit",
                    chrono::Utc::now().format("%Y%m%d")
                ),
                public_key,
                private_key,
                file_base64,
                warning,
            }
        }
        Err(e) => Response::err("RecoveryKeyError", format!("{e:#}")),
    }
}

/// Nome do servidor para o arquivo: o hostname, só com letras/dígitos/`-`/`.`.
fn server_name() -> String {
    let raw = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let clean: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
        .collect();
    if clean.is_empty() {
        "servidor".to_string()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AppState` de teste com um `config.toml` gravável (onde a pública é gravada).
    async fn state_with_config() -> (AppState, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("rustploy-kit-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&tmp).unwrap();
        let config = crate::platform::config::RustployConfig::default();
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let db = std::sync::Arc::new(crate::store::connect(&tmp).await.unwrap());
        let mut secrets = crate::platform::secrets::SecretsManager::new(db.clone()).unwrap();
        secrets.set_config_path_for_tests(config_path.clone());
        let tls = std::sync::Arc::new(
            crate::features::ingress::TlsManager::new(
                tmp.join("certs"),
                config.ingress.acme.clone(),
            )
            .unwrap(),
        );
        let state = AppState::new(
            db,
            std::sync::Arc::new(crate::engine::DockerClient::for_tests()),
            std::sync::Arc::new(crate::features::ingress::IngressController::new()),
            std::sync::Arc::new(crate::platform::event_bus::EventBus::new()),
            std::sync::Arc::new(secrets),
            tls,
            tmp.join("db"),
            tmp.join("backup"),
            30,
            config.api,
            &config.update,
            None,
            None,
        );
        (state, config_path)
    }

    fn error_code(response: &Response) -> Option<&str> {
        match response {
            Response::Err { code, .. } => Some(code.as_str()),
            _ => None,
        }
    }

    /// Repositório faltando, SSH, conta faltando ou inexistente: recusa ANTES de gerar, e o
    /// `config.toml` continua sem chave de recuperação.
    #[tokio::test]
    async fn pedido_invalido_nao_gera_nem_troca_a_chave() {
        let (state, config_path) = state_with_config().await;
        let cases: [(&str, Option<&str>, &str); 4] = [
            ("", None, "InvalidInput"),
            ("git@github.com:org/infra.git", Some("p1"), "InvalidInput"),
            ("https://github.com/org/infra.git", None, "InvalidInput"),
            (
                "https://github.com/org/infra.git",
                Some("nao-existe"),
                "NotFound",
            ),
        ];
        for (remote, provider, expected) in cases {
            let response = handle(
                &state,
                false,
                remote.to_string(),
                None,
                provider.map(str::to_string),
            )
            .await;
            assert_eq!(
                error_code(&response),
                Some(expected),
                "{remote} → {response:?}"
            );
        }
        assert!(!state.secrets.has_recovery());
        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(!config.contains("recovery_recipient = \"age1"), "{config}");
    }

    /// Uma conta PAT do GitHub com token cifrado no store.
    async fn add_provider(state: &AppState, id: &str, auth_mode: &str) {
        let provider = crate::store::git_providers::StoredProvider {
            id: id.into(),
            kind: "github".into(),
            name: format!("conta {id}"),
            base_url: String::new(),
            auth_mode: auth_mode.into(),
            oauth_client_id: None,
            oauth_client_secret_enc: None,
            access_token_enc: Some(state.secrets.encrypt("ghp_token_de_teste").unwrap()),
            refresh_token_enc: None,
            account_login: None,
            account_avatar: None,
            created_at: chrono::Utc::now(),
        };
        crate::store::git_providers::insert(&state.db, &provider)
            .await
            .unwrap();
    }

    /// O token da conta entra no arquivo quando o repositório é do host dela; de outro host, é
    /// recusado sem gerar a chave (o arquivo não pode levar o token do GitHub para outro servidor).
    #[tokio::test]
    async fn token_da_conta_so_entra_para_o_host_dela() {
        let (state, _) = state_with_config().await;
        add_provider(&state, "gh", "pat").await;

        let response = handle(
            &state,
            false,
            "https://gitlab.exemplo.com/org/infra.git".into(),
            None,
            Some("gh".into()),
        )
        .await;
        assert_eq!(error_code(&response), Some("InvalidInput"), "{response:?}");
        assert!(!state.secrets.has_recovery());

        let response = handle(
            &state,
            false,
            "https://github.com/org/infra.git".into(),
            None,
            Some("gh".into()),
        )
        .await;
        let Response::RecoveryKeyGenerated {
            file_base64,
            warning,
            ..
        } = response
        else {
            panic!("esperava RecoveryKeyGenerated: {response:?}");
        };
        assert!(warning.is_none(), "PAT não avisa");
        let bytes = base64::prelude::BASE64_STANDARD
            .decode(file_base64)
            .unwrap();
        let kit = RecoveryKit::decode(&bytes).unwrap();
        assert_eq!(kit.infra_token, "ghp_token_de_teste");
        assert_eq!(kit.infra_branch, "");
    }

    /// Conta OAuth: gera, mas avisa que o token pode expirar.
    #[tokio::test]
    async fn conta_oauth_gera_com_aviso() {
        let (state, _) = state_with_config().await;
        add_provider(&state, "oa", "oauth").await;
        let response = handle(
            &state,
            false,
            "https://github.com/org/infra.git".into(),
            Some("main".into()),
            Some("oa".into()),
        )
        .await;
        let Response::RecoveryKeyGenerated { warning, .. } = response else {
            panic!("esperava RecoveryKeyGenerated: {response:?}");
        };
        assert!(warning.unwrap_or_default().contains("OAuth"));
    }

    /// Remoto local: sem conta, e o arquivo devolvido abre com o repositório, a branch e a chave.
    #[tokio::test]
    async fn remoto_local_gera_o_arquivo_com_os_campos() {
        let (state, _) = state_with_config().await;
        let response = handle(
            &state,
            false,
            "file:///srv/infra.git".into(),
            Some(" estado ".into()),
            None,
        )
        .await;
        let Response::RecoveryKeyGenerated {
            public_key,
            private_key,
            file_base64,
            suggested_filename,
            warning,
        } = response
        else {
            panic!("esperava RecoveryKeyGenerated: {response:?}");
        };
        assert!(
            suggested_filename.ends_with(".rpkit"),
            "{suggested_filename}"
        );
        assert!(warning.is_none());
        let bytes = base64::prelude::BASE64_STANDARD
            .decode(file_base64)
            .unwrap();
        let kit = RecoveryKit::decode(&bytes).unwrap();
        assert_eq!(kit.infra_remote, "file:///srv/infra.git");
        assert_eq!(kit.infra_branch, "estado");
        assert_eq!(kit.infra_token, "");
        assert_eq!(kit.private_key, private_key);
        assert_eq!(kit.public_key, public_key);
    }
}
