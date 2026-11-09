//! Cifra no lugar o que a migração SQL → YAML deixou em claro.
//!
//! A migração (`migrate.rs`) copiou os tokens de webhook da tabela antiga como estavam, e os
//! `client_secret` de provider podem ter vindo em claro de versões anteriores. O código atual
//! cifra tudo ao gravar, mas nada cifrava o legado — e o `InfraAudit` classifica esses valores
//! como **certeza**, o que impede ligar a sincronização com o git (`PlaintextFound`).
//!
//! Aqui o valor **não muda**: o mesmo token de webhook (a URL cadastrada no provedor segue
//! valendo) e o mesmo `client_secret`, só que como `ENC[age:…]`. Roda no boot, é idempotente
//! (o que já está cifrado fica como está) e as leituras de hoje já aceitam os dois formatos.

use super::model::InfraState;
use super::repo::Repo;
use crate::platform::secrets::{SecretsManager, is_enc};
use anyhow::Result;

/// Cifra os tokens de webhook e os `client_secret` de provider que estão em claro. Devolve
/// quantos valores foram cifrados (0 = nada a fazer, e nada é gravado).
///
/// Mesmos critérios de `audit::plaintext_findings`: o que o audit aponta como "sem cifra" é o que
/// esta função cifra.
pub fn seal_plaintext(repo: &Repo, secrets: &SecretsManager) -> Result<usize> {
    // Antes de qualquer infra gravada não há o que cifrar; e `update` gravaria um estado vazio.
    if !repo.is_initialized()? {
        return Ok(0);
    }
    // Olhar antes evita tomar a trava de escrita (e gravar) quando está tudo cifrado.
    if count_plaintext(&repo.snapshot()) == 0 {
        return Ok(0);
    }
    repo.update(|st| {
        let mut sealed = 0;
        for prov in &mut st.providers {
            if let Some(v) = prov.oauth_client_secret_enc.as_mut()
                && !is_enc(v)
            {
                *v = secrets.encrypt(v)?;
                sealed += 1;
            }
        }
        for svc in st.projects.iter_mut().flat_map(|p| p.services.iter_mut()) {
            if let Some(v) = svc.webhook_token.as_mut()
                && !is_enc(v)
            {
                *v = secrets.encrypt(v)?;
                sealed += 1;
            }
        }
        Ok(sealed)
    })
}

fn count_plaintext(st: &InfraState) -> usize {
    let providers = st
        .providers
        .iter()
        .filter(|p| {
            p.oauth_client_secret_enc
                .as_deref()
                .is_some_and(|v| !is_enc(v))
        })
        .count();
    let webhooks = st
        .projects
        .iter()
        .flat_map(|p| p.services.iter())
        .filter(|s| s.webhook_token.as_deref().is_some_and(|t| !is_enc(t)))
        .count();
    providers + webhooks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::webhook_tokens;
    use crate::store::{Db, infra::audit::plaintext_findings, infra::model::ProviderInfra};
    use std::sync::Arc;

    fn provider(secret: Option<&str>) -> ProviderInfra {
        ProviderInfra {
            id: "gp1".into(),
            kind: "github".into(),
            name: "GitHub".into(),
            base_url: "https://github.com".into(),
            auth_mode: "oauth".into(),
            oauth_client_id: Some("cid".into()),
            oauth_client_secret_enc: secret.map(Into::into),
            account_login: None,
            account_avatar: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// Banco com um projeto, dois serviços com token de webhook em claro e um provider em claro.
    fn legacy() -> (Arc<Db>, SecretsManager) {
        let db = Arc::new(Db::lazy_for_test());
        db.seed_project_for_test("p1");
        db.seed_service_for_test("p1", "s1");
        db.seed_service_for_test("p1", "s2");
        db.infra()
            .update(|st| {
                st.providers.push(provider(Some("segredo-oauth")));
                for s in st.projects[0].services.iter_mut() {
                    s.webhook_token = Some(format!("tok-{}", s.id));
                }
                Ok(())
            })
            .unwrap();
        let secrets = SecretsManager::new(db.clone()).unwrap();
        (db, secrets)
    }

    #[tokio::test]
    async fn cifra_o_legado_sem_mudar_o_valor() {
        let (db, secrets) = legacy();
        assert_eq!(plaintext_findings(&db.infra().snapshot()).len(), 3);

        let n = seal_plaintext(db.infra(), &secrets).unwrap();
        assert_eq!(n, 3);

        // o audit não acha mais nada: dá para ligar o git
        assert_eq!(plaintext_findings(&db.infra().snapshot()), vec![]);

        // o valor é o mesmo: o webhook continua casando com o token que o provedor já tem
        assert_eq!(
            webhook_tokens::get(&db, &secrets, "s1")
                .await
                .unwrap()
                .as_deref(),
            Some("tok-s1")
        );
        assert_eq!(
            webhook_tokens::get(&db, &secrets, "s2")
                .await
                .unwrap()
                .as_deref(),
            Some("tok-s2")
        );
        let st = db.infra().snapshot();
        assert_eq!(
            secrets
                .decrypt(st.providers[0].oauth_client_secret_enc.as_deref().unwrap())
                .unwrap(),
            "segredo-oauth"
        );
    }

    #[tokio::test]
    async fn idempotente_e_nao_regrava_o_que_ja_esta_cifrado() {
        let (db, secrets) = legacy();
        assert_eq!(seal_plaintext(db.infra(), &secrets).unwrap(), 3);
        let depois = db.infra().snapshot();

        // segunda rodada: nada a cifrar, e o estado (inclusive o texto cifrado, que muda a cada
        // `encrypt`) fica exatamente como estava
        assert_eq!(seal_plaintext(db.infra(), &secrets).unwrap(), 0);
        assert_eq!(*db.infra().snapshot(), *depois);
    }

    #[tokio::test]
    async fn sem_infra_gravada_nao_faz_nada() {
        let db = Arc::new(Db::lazy_for_test());
        let secrets = SecretsManager::new(db.clone()).unwrap();
        assert_eq!(seal_plaintext(db.infra(), &secrets).unwrap(), 0);
        assert!(!db.infra().is_initialized().unwrap());
    }

    #[tokio::test]
    async fn provider_sem_secret_e_servico_sem_webhook_ficam_como_estao() {
        let db = Arc::new(Db::lazy_for_test());
        db.seed_project_for_test("p1");
        db.seed_service_for_test("p1", "s1"); // sem webhook_token
        db.infra()
            .update(|st| {
                st.providers.push(provider(None)); // PAT, sem client_secret
                Ok(())
            })
            .unwrap();
        let secrets = SecretsManager::new(db.clone()).unwrap();
        assert_eq!(seal_plaintext(db.infra(), &secrets).unwrap(), 0);
        let st = db.infra().snapshot();
        assert!(st.providers[0].oauth_client_secret_enc.is_none());
        assert!(st.projects[0].services[0].webhook_token.is_none());
    }
}
