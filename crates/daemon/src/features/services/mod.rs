//! Ciclo de vida de um serviço: criar, atualizar (com a sincronização do firewall) e remover.

mod context;
pub mod create;
pub mod delete;
pub mod update;

pub use context::ServicesContext;

/// `(id, nome, depends_on)` de todos os serviços do projeto, para a validação de dependências.
pub(crate) async fn project_deps(
    db: &crate::store::Db,
    project_id: &str,
) -> anyhow::Result<Vec<(String, String, Vec<String>)>> {
    Ok(crate::store::services::list(db, project_id)
        .await?
        .into_iter()
        .map(|s| (s.id, s.spec.name, s.spec.depends_on))
        .collect())
}

/// Valida o `depends_on` de um spec que vai ser gravado (`self_id`: `None` na criação).
pub(crate) async fn check_depends_on(
    db: &crate::store::Db,
    spec: &shared::ServiceSpec,
    self_id: Option<&str>,
) -> Result<(), shared::Response> {
    if spec.depends_on.is_empty() {
        return Ok(());
    }
    let others = project_deps(db, &spec.project_id)
        .await
        .map_err(|e| shared::Response::err("DatabaseError", e.to_string()))?;
    crate::features::deploy::deps::validate(&others, self_id, &spec.depends_on)
        .map_err(|e| shared::Response::err(e.code, e.message))?;

    // Um ciclo que passa pelo banco compartilhado (entre projetos) só aparece no grafo completo.
    let all = crate::store::services::list_all(db)
        .await
        .map_err(|e| shared::Response::err("DatabaseError", e.to_string()))?;
    let access = crate::store::shared_access::all(db)
        .await
        .unwrap_or_default();
    let implicit = crate::features::deploy::autostart::implicit_edges(&access, &all);
    let tuples: Vec<(String, String, Vec<String>)> = all
        .into_iter()
        .map(|s| (s.id, s.spec.name, s.spec.depends_on))
        .collect();
    crate::features::deploy::deps::shared_cycle(&tuples, &implicit, self_id, &spec.depends_on)
        .map_err(|nomes| {
            shared::Response::err(
                "DependencyCycle",
                format!(
                    "essa dependência cria um ciclo que passa por um banco compartilhado entre: {}",
                    if nomes.is_empty() {
                        "(o serviço novo)".to_string()
                    } else {
                        nomes.join(", ")
                    }
                ),
            )
        })
}
