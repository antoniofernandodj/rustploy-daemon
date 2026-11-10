//! Retenção de tags do registry embutido: antes de cada GC, apaga as tags mais antigas de cada
//! repositório e mantém as `keep_last` mais recentes.
//!
//! Existe porque o GC só varre o que **ninguém aponta**, e uma tag aponta para sempre: um
//! workflow que publica uma tag por commit (`flow-client:<sha>`) acumula uma imagem por push sem
//! que nada as solte. Com a retenção ligada, a tag velha sai aqui; o manifest e as camadas que
//! ficarem órfãs saem no GC, logo em seguida.
//!
//! Regras de segurança:
//! - **nunca** apaga uma tag que um serviço de fonte Registry usa (`push_deploy::tags_in_use`),
//!   mesmo que ela seja a mais antiga;
//! - apaga só a linha da tag (`delete_tag_only`), nunca por digest: um rebuild idêntico dá o mesmo
//!   digest a duas tags, e apagar por digest levaria junto a que ainda vale;
//! - sem a lista de serviços (falha de leitura), não apaga nada;
//! - desligada por padrão (`RegistryRetentionConfig::default`).

use std::collections::HashSet;

use anyhow::Result;
use chrono::{DateTime, Utc};
use shared::RegistryRetentionConfig;
use tracing::info;

use crate::store::daemon_settings::{self, KEY_REGISTRY_RETENTION};
use crate::store::{Db, registry as db_registry};

/// A política gravada; ausente ou ilegível vale o padrão (desligada).
pub async fn load(db: &Db) -> RegistryRetentionConfig {
    match daemon_settings::get(db, KEY_REGISTRY_RETENTION).await {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => RegistryRetentionConfig::default(),
    }
}

pub async fn save(db: &Db, config: &RegistryRetentionConfig) -> Result<()> {
    let json = serde_json::to_string(config)?;
    daemon_settings::set(db, KEY_REGISTRY_RETENTION, &json).await
}

/// Quais tags de **um** repositório saem. Pura, para ser testável sem banco.
///
/// As tags são ordenadas da mais recente para a mais antiga (empate: nome em ordem decrescente, só
/// para o resultado ser estável). Ficam as `keep_last` primeiras e as que estão em uso; o resto sai.
pub fn tags_to_delete(
    tags: &[(String, DateTime<Utc>)],
    in_use: &HashSet<String>,
    keep_last: u32,
) -> Vec<String> {
    let mut sorted: Vec<&(String, DateTime<Utc>)> = tags.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    sorted
        .into_iter()
        .enumerate()
        .filter(|(i, (tag, _))| *i >= keep_last.max(1) as usize && !in_use.contains(tag))
        .map(|(_, (tag, _))| tag.clone())
        .collect()
}

/// Aplica a política a todos os repositórios. `in_use` são pares `(repo, tag)` que ficam. Devolve
/// quantas tags foram apagadas.
pub async fn apply(
    db: &Db,
    config: &RegistryRetentionConfig,
    in_use: &HashSet<(String, String)>,
) -> Result<usize> {
    if !config.enabled {
        return Ok(0);
    }
    let mut removed = 0;
    for name in db_registry::list_repo_names(db).await? {
        let Some(repo) = db_registry::get_repo_by_name(db, &name).await? else {
            continue;
        };
        let tags: Vec<(String, DateTime<Utc>)> = db_registry::list_tags_detailed(db, &repo.id)
            .await?
            .into_iter()
            .map(|t| (t.tag, t.updated_at))
            .collect();
        let protected: HashSet<String> = in_use
            .iter()
            .filter(|(r, _)| r == &name)
            .map(|(_, t)| t.clone())
            .collect();
        for tag in tags_to_delete(&tags, &protected, config.effective_keep_last()) {
            if db_registry::delete_tag_only(db, &repo.id, &tag).await? {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// O que o job diário e o botão de GC chamam antes do GC: lê a política e, se estiver ligada,
/// levanta as tags em uso pelos serviços e aplica. Qualquer falha ao ler os serviços aborta sem
/// apagar nada.
pub async fn apply_configured(db: &Db) -> Result<usize> {
    let config = load(db).await;
    if !config.enabled {
        return Ok(0);
    }
    let services = crate::store::services::list_all(db).await?;
    let port = crate::platform::config::RustployConfig::global()
        .registry
        .port;
    let domain = super::push_deploy::embedded_domain(db).await;
    let in_use = super::push_deploy::tags_in_use(&services, port, domain.as_deref());
    let removed = apply(db, &config, &in_use).await?;
    if removed > 0 {
        info!(
            tags = removed,
            keep_last = config.effective_keep_last(),
            "registry: retenção apagou tags antigas"
        );
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use ulid::Ulid;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(minutes)
    }

    fn tags(spec: &[(&str, i64)]) -> Vec<(String, DateTime<Utc>)> {
        spec.iter().map(|(t, m)| (t.to_string(), at(*m))).collect()
    }

    fn usadas(t: &[&str]) -> HashSet<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mantem_as_n_mais_recentes() {
        let t = tags(&[("a", 1), ("b", 2), ("c", 3), ("d", 4)]);
        let mut out = tags_to_delete(&t, &HashSet::new(), 2);
        out.sort();
        assert_eq!(out, vec!["a", "b"]);
    }

    #[test]
    fn menos_tags_que_o_limite_nao_apaga_nada() {
        let t = tags(&[("a", 1), ("b", 2)]);
        assert!(tags_to_delete(&t, &HashSet::new(), 10).is_empty());
    }

    #[test]
    fn tag_em_uso_nunca_sai_mesmo_a_mais_antiga() {
        let t = tags(&[("staging", 1), ("x", 2), ("y", 3), ("z", 4)]);
        let mut out = tags_to_delete(&t, &usadas(&["staging"]), 1);
        out.sort();
        assert_eq!(out, vec!["x", "y"]);
    }

    #[test]
    fn keep_last_zero_vale_um() {
        let t = tags(&[("a", 1), ("b", 2)]);
        assert_eq!(tags_to_delete(&t, &HashSet::new(), 0), vec!["a"]);
    }

    #[test]
    fn empate_de_data_e_estavel() {
        let t = tags(&[("a", 5), ("b", 5), ("c", 5)]);
        assert_eq!(tags_to_delete(&t, &HashSet::new(), 1), vec!["b", "a"]);
    }

    async fn test_db() -> Db {
        let dir = std::env::temp_dir().join(format!("rustploy_test_retention_{}", Ulid::new()));
        crate::store::connect(&dir).await.unwrap()
    }

    /// Cria um repo com tags apontando para manifests (todos com o MESMO digest, como num rebuild
    /// idêntico) e confere que a retenção apaga só as linhas das tags antigas.
    #[tokio::test]
    async fn desligada_nao_apaga_e_ligada_apaga_so_as_tags_sem_levar_o_digest_junto() {
        let db = test_db().await;
        let repo = db_registry::get_or_create_repo(&db, "app").await.unwrap();
        let digest = "sha256:aaaa";
        db_registry::insert_blob(&db, digest, 1).await.unwrap();
        db_registry::insert_manifest(
            &db,
            digest,
            &repo.id,
            "application/vnd.oci.image.manifest.v1+json",
            1,
            &[],
        )
        .await
        .unwrap();
        for t in ["v1", "v2", "v3", "staging"] {
            db_registry::upsert_tag(&db, &repo.id, t, digest)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let in_use: HashSet<(String, String)> = [("app".to_string(), "v1".to_string())]
            .into_iter()
            .collect();

        let desligada = RegistryRetentionConfig::default();
        assert_eq!(apply(&db, &desligada, &in_use).await.unwrap(), 0);

        let ligada = RegistryRetentionConfig {
            enabled: true,
            keep_last: 1,
        };
        // recentes: staging > v3 > v2 > v1. Fica staging (a mais nova) e v1 (em uso); saem v2 e v3.
        assert_eq!(apply(&db, &ligada, &in_use).await.unwrap(), 2);
        let mut restantes = db_registry::list_tags(&db, &repo.id).await.unwrap();
        restantes.sort();
        assert_eq!(restantes, vec!["staging", "v1"]);
        // o manifest (mesmo digest das que ficaram) continua lá
        assert!(
            db_registry::get_manifest(&db, &repo.id, digest)
                .await
                .unwrap()
                .is_some()
        );
    }
}
