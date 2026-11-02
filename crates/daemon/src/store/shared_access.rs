//! Tabela `shared_access`: quais projetos podem alcançar cada servidor de banco
//! compartilhado.

use super::Db;
use super::infra::model::SharedAccessInfra;
use anyhow::Result;
use chrono::Utc;

// Os acessos moram no YAML de infra, dentro do projeto que **recebe** o acesso
// (`ProjectInfra.shared_access`). A tabela antiga `shared_access` não é mais lida nem escrita.

pub async fn grant(db: &Db, server_id: &str, project_id: &str) -> Result<()> {
    db.infra().update(|st| {
        let Some(p) = st.projects.iter_mut().find(|p| p.id == project_id) else {
            anyhow::bail!("projeto {project_id} não encontrado");
        };
        if !p
            .shared_access
            .iter()
            .any(|a| a.server_service_id == server_id)
        {
            p.shared_access.push(SharedAccessInfra {
                server_service_id: server_id.to_string(),
                created_at: Utc::now(),
            });
        }
        Ok(())
    })
}

pub async fn revoke(db: &Db, server_id: &str, project_id: &str) -> Result<bool> {
    db.infra().update(|st| {
        let Some(p) = st.projects.iter_mut().find(|p| p.id == project_id) else {
            return Ok(false);
        };
        let before = p.shared_access.len();
        p.shared_access.retain(|a| a.server_service_id != server_id);
        Ok(p.shared_access.len() != before)
    })
}

/// `(servidor, projeto, concedido em)` de todos os acessos, em ordem de concessão.
fn rows(db: &Db) -> Vec<(String, String, chrono::DateTime<Utc>)> {
    let st = db.infra().snapshot();
    let mut v: Vec<_> = st
        .projects
        .iter()
        .flat_map(|p| {
            p.shared_access
                .iter()
                .map(move |a| (a.server_service_id.clone(), p.id.clone(), a.created_at))
        })
        .collect();
    v.sort_by(|a, b| (a.2, &a.1).cmp(&(b.2, &b.1)));
    v
}

/// Projetos autorizados num servidor, em ordem de concessão.
pub async fn projects_of(db: &Db, server_id: &str) -> Result<Vec<String>> {
    Ok(rows(db)
        .into_iter()
        .filter(|(s, _, _)| s == server_id)
        .map(|(_, p, _)| p)
        .collect())
}

pub async fn delete_for_server(db: &Db, server_id: &str) -> Result<()> {
    db.infra().update(|st| {
        for p in &mut st.projects {
            p.shared_access.retain(|a| a.server_service_id != server_id);
        }
        Ok(())
    })
}

/// Todos os acessos concedidos: `(server_service_id, project_id)`. Alimenta as dependências
/// implícitas do autostart (todo serviço de um projeto autorizado espera o servidor de banco).
pub async fn all(db: &Db) -> Result<Vec<(String, String)>> {
    Ok(rows(db).into_iter().map(|(s, p, _)| (s, p)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grant_e_idempotente_e_revoke_remove() {
        let db = Db::memory_for_test().await;
        for p in ["prj_1", "prj_2", "srv"] {
            db.seed_project_for_test(p);
        }
        db.seed_service_for_test("srv", "svc_a");
        assert!(grant(&db, "svc_fantasma", "prj_1").await.is_err());
        grant(&db, "svc_a", "prj_1").await.unwrap();
        grant(&db, "svc_a", "prj_1").await.unwrap();
        grant(&db, "svc_a", "prj_2").await.unwrap();
        assert_eq!(projects_of(&db, "svc_a").await.unwrap().len(), 2);
        assert!(revoke(&db, "svc_a", "prj_1").await.unwrap());
        assert!(!revoke(&db, "svc_a", "prj_1").await.unwrap());
        assert_eq!(projects_of(&db, "svc_a").await.unwrap(), vec!["prj_2"]);
        delete_for_server(&db, "svc_a").await.unwrap();
        assert!(projects_of(&db, "svc_a").await.unwrap().is_empty());
    }
}
