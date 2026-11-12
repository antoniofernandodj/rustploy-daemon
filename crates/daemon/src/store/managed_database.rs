//! Tabela `managed_database`: databases criados em servidores compartilhados.
//! A senha fica aqui **cifrada** (`ENC[age:…]`, como os demais segredos do estado) para a migração
//! poder usá-la como destino: quem precisa do valor chama [`open`]. Um registro antigo, em claro,
//! passa como está e é cifrado no boot (`infra::seal`).

use super::Db;
use super::infra::model::ManagedDatabaseInfra;
use crate::platform::secrets::{SecretsManager, is_enc};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ulid::Ulid;

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub server_service_id: String,
    pub project_id: String,
    pub name: String,
    pub password: String,
    pub env_var: String,
    pub connection_limit: Option<u32>,
    pub statement_timeout_ms: Option<u32>,
    pub created_at: DateTime<Utc>,
}

// Os bancos gerenciados moram no YAML de infra, dentro do projeto **consumidor**
// (`ProjectInfra.managed_databases`). A tabela antiga `managed_database` não é mais lida nem
// escrita. Atenção: `password` está em claro até a Fase 5 (ver `infra::model`).

/// A linha com a senha **aberta** (em claro, em memória). O que está no estado é `ENC[age:…]`;
/// um valor em claro (legado, ainda não selado) passa como está.
pub fn open(secrets: &SecretsManager, mut r: Row) -> Result<Row> {
    if is_enc(&r.password) {
        r.password = secrets
            .decrypt(&r.password)
            .map_err(|e| anyhow::anyhow!("senha do database '{}': {e}", r.name))?;
    }
    Ok(r)
}

fn to_row(project_id: &str, m: &ManagedDatabaseInfra) -> Row {
    Row {
        id: m.id.clone(),
        server_service_id: m.server_service_id.clone(),
        project_id: project_id.to_string(),
        name: m.name.clone(),
        password: m.password.clone(),
        env_var: m.env_var.clone(),
        connection_limit: m.connection_limit,
        statement_timeout_ms: m.statement_timeout_ms,
        created_at: m.created_at,
    }
}

fn all_rows(db: &Db) -> Vec<Row> {
    let st = db.infra().snapshot();
    let mut v: Vec<Row> = st
        .projects
        .iter()
        .flat_map(|p| p.managed_databases.iter().map(move |m| to_row(&p.id, m)))
        .collect();
    v.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    v
}

pub async fn insert(db: &Db, r: &Row) -> Result<Row> {
    let id = format!("mdb_{}", Ulid::new());
    let rec = ManagedDatabaseInfra {
        id: id.clone(),
        server_service_id: r.server_service_id.clone(),
        name: r.name.clone(),
        password: r.password.clone(),
        env_var: r.env_var.clone(),
        connection_limit: r.connection_limit,
        statement_timeout_ms: r.statement_timeout_ms,
        created_at: r.created_at,
    };
    // A validação do repositório recusa (servidor, nome) repetido — era o `UNIQUE` do SQL — e
    // servidor que não existe.
    db.infra().update(|st| {
        let Some(p) = st.projects.iter_mut().find(|p| p.id == r.project_id) else {
            anyhow::bail!("projeto {} não encontrado", r.project_id);
        };
        p.managed_databases.push(rec);
        Ok(())
    })?;
    Ok(Row { id, ..r.clone() })
}

pub async fn list(db: &Db, server_id: &str) -> Result<Vec<Row>> {
    Ok(all_rows(db)
        .into_iter()
        .filter(|r| r.server_service_id == server_id)
        .collect())
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Row>> {
    Ok(all_rows(db).into_iter().find(|r| r.id == id))
}

pub async fn delete(db: &Db, id: &str) -> Result<()> {
    db.infra().update(|st| {
        for p in &mut st.projects {
            p.managed_databases.retain(|m| m.id != id);
        }
        Ok(())
    })
}

pub async fn delete_for_server(db: &Db, server_id: &str) -> Result<()> {
    db.infra().update(|st| {
        for p in &mut st.projects {
            p.managed_databases
                .retain(|m| m.server_service_id != server_id);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(server: &str, project: &str, name: &str) -> Row {
        Row {
            id: String::new(),
            server_service_id: server.into(),
            project_id: project.into(),
            name: name.into(),
            password: "pw".into(),
            env_var: "DATABASE_URL".into(),
            connection_limit: Some(5),
            statement_timeout_ms: None,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn crud_unicidade_e_limpeza_por_servidor() {
        let db = Db::memory_for_test().await;
        db.seed_project_for_test("srv");
        db.seed_project_for_test("cli");
        db.seed_service_for_test("srv", "svc_pg");

        let a = insert(&db, &row("svc_pg", "cli", "loja")).await.unwrap();
        assert!(a.id.starts_with("mdb_"));
        assert!(
            insert(&db, &row("svc_pg", "cli", "loja")).await.is_err(),
            "(servidor, nome) é único"
        );
        assert!(
            insert(&db, &row("svc_fantasma", "cli", "x")).await.is_err(),
            "servidor inexistente"
        );
        insert(&db, &row("svc_pg", "cli", "blog")).await.unwrap();
        assert_eq!(list(&db, "svc_pg").await.unwrap().len(), 2);
        let got = get(&db, &a.id).await.unwrap().unwrap();
        assert_eq!(
            (got.project_id.as_str(), got.connection_limit),
            ("cli", Some(5))
        );
        delete(&db, &a.id).await.unwrap();
        assert!(get(&db, &a.id).await.unwrap().is_none());
        delete_for_server(&db, "svc_pg").await.unwrap();
        assert!(list(&db, "svc_pg").await.unwrap().is_empty());
    }
}
