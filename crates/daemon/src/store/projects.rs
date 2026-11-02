//! Tabela `project`: CRUD de projetos e suas env vars de nível de projeto.
//!
//! Desde a Fase 4d do plano `docs/plano-backup-estado-em-git.md` os projetos moram no **YAML de
//! infra** (`store::infra`), não mais no SQLite: leituras vêm do estado em memória e cada escrita
//! é uma transação do repositório. Apagar um projeto leva junto tudo o que é dele no YAML
//! (serviços, jobs, secrets, bancos e acessos).

use super::Db;
use super::infra::model::ProjectInfra;
use anyhow::Result;
use chrono::Utc;
use shared::{EnvComment, EnvVar, Project};
use ulid::Ulid;

fn to_project(p: &ProjectInfra) -> Project {
    Project {
        id: p.id.clone(),
        name: p.name.clone(),
        description: p.description.clone(),
        env_vars: p.env_vars.clone(),
        env_comments: p.env_comments.clone(),
        created_at: p.created_at,
    }
}

pub async fn create(db: &Db, name: String, description: Option<String>) -> Result<Project> {
    let id = format!("prj_{}", Ulid::new());
    let now = Utc::now();
    // Rede gravada na criação (ID inteiro: não colide) e lida daí em diante.
    let network_name = shared::new_project_network_name(&id);
    let rec = ProjectInfra {
        id,
        name,
        description,
        env_vars: vec![],
        env_comments: vec![],
        network_name: Some(network_name),
        created_at: now,
        services: vec![],
        jobs: vec![],
        secrets: vec![],
        managed_databases: vec![],
        shared_access: vec![],
    };
    let out = to_project(&rec);
    // A validação do repositório recusa nome repetido (era o `UNIQUE(name)` do SQL).
    db.infra().update(|st| {
        st.projects.push(rec);
        Ok(())
    })?;
    Ok(out)
}

pub async fn update_env_vars(
    db: &Db,
    id: &str,
    env_vars: Vec<EnvVar>,
    env_comments: Vec<EnvComment>,
) -> Result<Option<Project>> {
    db.infra().update(|st| {
        Ok(st.projects.iter_mut().find(|p| p.id == id).map(|p| {
            p.env_vars = env_vars;
            p.env_comments = env_comments;
            to_project(p)
        }))
    })
}

/// Nome da rede Docker do projeto — o gravado. Sem gravado (banco de antes da
/// migração, ou linha inserida por fora, como faz o importer), a fórmula
/// legada, que é o nome que a rede já tem.
pub async fn network_name(db: &Db, project_id: &str) -> Result<String> {
    let st = db.infra().snapshot();
    Ok(st
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .and_then(|p| p.network_name.clone())
        .unwrap_or_else(|| shared::legacy_project_net_for(project_id)))
}

/// `id do projeto → nome da rede` de todos os projetos (o inventário Docker
/// descobre por aqui de quem é cada rede `rp_net_*`).
pub async fn network_names(db: &Db) -> Result<std::collections::HashMap<String, String>> {
    let st = db.infra().snapshot();
    Ok(st
        .projects
        .iter()
        .map(|p| {
            let net = p
                .network_name
                .clone()
                .unwrap_or_else(|| shared::legacy_project_net_for(&p.id));
            (p.id.clone(), net)
        })
        .collect())
}

pub async fn list(db: &Db) -> Result<Vec<Project>> {
    Ok(db
        .infra()
        .snapshot()
        .projects
        .iter()
        .map(to_project)
        .collect())
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Project>> {
    Ok(db
        .infra()
        .snapshot()
        .projects
        .iter()
        .find(|p| p.id == id)
        .map(to_project))
}

pub async fn update(
    db: &Db,
    id: &str,
    name: String,
    description: Option<String>,
) -> Result<Option<Project>> {
    db.infra().update(|st| {
        Ok(st.projects.iter_mut().find(|p| p.id == id).map(|p| {
            p.name = name;
            p.description = description;
            to_project(p)
        }))
    })
}

pub async fn delete(db: &Db, id: &str) -> Result<bool> {
    db.infra().update(|st| {
        let before = st.projects.len();
        st.projects.retain(|p| p.id != id);
        Ok(st.projects.len() != before)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::EnvVarValue;

    async fn mem_db() -> Db {
        Db::memory_for_test().await
    }

    #[tokio::test]
    async fn crud_nome_unico_rede_gravada_e_cascata() {
        let db = mem_db().await;
        let a = create(&db, "App".into(), Some("d".into())).await.unwrap();
        assert!(
            create(&db, "App".into(), None).await.is_err(),
            "nome repetido"
        );
        let b = create(&db, "Outro".into(), None).await.unwrap();
        // renomear para um nome em uso também é recusado, e nada muda
        assert!(update(&db, &b.id, "App".into(), None).await.is_err());
        assert_eq!(get(&db, &b.id).await.unwrap().unwrap().name, "Outro");
        // lista em ordem de criação
        let nomes: Vec<_> = list(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(nomes, ["App", "Outro"]);
        // rede gravada na criação
        assert_eq!(
            network_name(&db, &a.id).await.unwrap(),
            shared::new_project_network_name(&a.id)
        );
        assert_eq!(network_names(&db).await.unwrap().len(), 2);
        // desconhecido → rede legada, update/get/delete → None/false
        assert_eq!(
            network_name(&db, "nada").await.unwrap(),
            shared::legacy_project_net_for("nada")
        );
        assert!(
            update(&db, "nada", "x".into(), None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            update_env_vars(&db, "nada", vec![], vec![])
                .await
                .unwrap()
                .is_none()
        );
        assert!(delete(&db, &a.id).await.unwrap());
        assert!(!delete(&db, &a.id).await.unwrap());
        assert!(get(&db, &a.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn env_vars_e_comentarios_persistem() {
        let db = mem_db().await;
        let proj = create(&db, "domain".into(), None).await.unwrap();

        let env_vars = vec![EnvVar {
            key: "JWT_SECRET".into(),
            value: EnvVarValue::Plain("abc123".into()),
        }];
        let env_comments = vec![EnvComment {
            text: "# segredo do app".into(),
            before_key: Some("JWT_SECRET".into()),
        }];

        update_env_vars(&db, &proj.id, env_vars.clone(), env_comments.clone())
            .await
            .unwrap();

        let got = get(&db, &proj.id).await.unwrap().unwrap();
        assert_eq!(got.env_vars, env_vars);
        assert_eq!(
            got.env_comments, env_comments,
            "comentário do .env não persistiu"
        );
    }
}
