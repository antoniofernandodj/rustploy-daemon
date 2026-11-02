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
        .map_err(|e| shared::Response::err(e.code, e.message))
}
