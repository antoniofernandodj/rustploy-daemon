//! `Command::GitProviderDelete`: desconecta (apaga) um provedor git.
//!
//! Recusa enquanto algum serviço ou job ainda aponta para a conta (`InUse`): sem ela o clone deles
//! falha no próximo deploy, e foi assim que uma troca de token deixou 13 serviços sem vínculo. Com
//! `force` remove mesmo assim (o cliente já avisou o usuário).

use crate::bootstrap::state::AppState;
use crate::store::Db;
use shared::{Response, ServiceSource};

/// Quem usa a conta: `projeto/serviço` e `projeto/job (job)`, em ordem estável.
pub async fn users_of(db: &Db, provider_id: &str) -> anyhow::Result<Vec<String>> {
    let projects: std::collections::HashMap<String, String> = crate::store::projects::list(db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let name_of = |project_id: &str| {
        projects
            .get(project_id)
            .cloned()
            .unwrap_or_else(|| project_id.to_string())
    };
    let mut out = vec![];
    for svc in crate::store::services::list_all(db).await? {
        if let ServiceSource::Git(g) = &svc.spec.source
            && g.provider_id.as_deref() == Some(provider_id)
        {
            out.push(format!(
                "{}/{}",
                name_of(&svc.spec.project_id),
                svc.spec.name
            ));
        }
    }
    for job in crate::store::job::list_all(db).await? {
        if job
            .git_source
            .as_ref()
            .and_then(|g| g.provider_id.as_deref())
            == Some(provider_id)
        {
            out.push(format!("{}/{} (job)", name_of(&job.project_id), job.name));
        }
    }
    out.sort();
    Ok(out)
}

pub async fn handle(state: AppState, id: String, force: bool) -> Response {
    if !force {
        match users_of(&state.db, &id).await {
            Ok(users) if !users.is_empty() => {
                return Response::err(
                    "InUse",
                    format!(
                        "esta conta é usada por {} item(ns): {}. Sem ela eles deixam de clonar o \
                         repositório; reaponte-os para outra conta antes, ou confirme a remoção.",
                        users.len(),
                        users.join(", ")
                    ),
                );
            }
            Ok(_) => {}
            Err(e) => return Response::err("DatabaseError", e.to_string()),
        }
    }
    match crate::store::git_providers::delete(&state.db, &id).await {
        Ok(true) => Response::Ok,
        Ok(false) => Response::err("NotFound", "Provider não encontrado"),
        Err(e) => Response::err("DatabaseError", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{GitSource, JobGitSource};

    #[tokio::test]
    async fn lista_quem_usa_a_conta_e_ignora_as_outras() {
        let db = Db::memory_for_test().await;
        db.seed_project_for_test("p1");
        db.seed_service_for_test("p1", "s1");
        db.seed_service_for_test("p1", "s2");
        db.infra()
            .update(|st| {
                let p = &mut st.projects[0];
                p.services[0].spec.source = ServiceSource::Git(GitSource {
                    provider_id: Some("gp_velha".into()),
                    ..GitSource::default()
                });
                p.services[1].spec.source = ServiceSource::Git(GitSource {
                    provider_id: Some("gp_outra".into()),
                    ..GitSource::default()
                });
                Ok(())
            })
            .unwrap();
        crate::store::job::create(
            &db,
            "p1",
            None,
            "backup",
            "x",
            Some(&JobGitSource {
                provider_id: Some("gp_velha".into()),
                url: "https://github.com/o/r.git".into(),
                branch: "main".into(),
                username: None,
                ..JobGitSource::default()
            }),
            "main",
            &[],
            &[],
            None,
        )
        .await
        .unwrap();

        let users = users_of(&db, "gp_velha").await.unwrap();
        assert_eq!(users.len(), 2, "{users:?}");
        assert!(users.iter().any(|u| u.ends_with("/backup (job)")));
        assert!(users.iter().any(|u| u.starts_with("p1/")));
        assert!(users_of(&db, "gp_nova").await.unwrap().is_empty());
    }
}
