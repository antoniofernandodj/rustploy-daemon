//! Validação dos **vínculos** de um spec que vai ser gravado (`ServiceSpec::links`).
//! Ver `docs/plano-recursos-e-vinculos.md`.
//!
//! Regras:
//! - o alvo existe, não é o próprio serviço e não aparece duas vezes;
//! - o prefixo, se escrito, é um nome de variável válido;
//! - do **mesmo projeto**, qualquer serviço; de **outro projeto**, só um banco compartilhado, e só se a
//!   trava dele (`shared.accept_links`) aceitar — a trava vale para vínculos **novos**: um que já
//!   existia continua mesmo com ela desligada;
//! - nenhum ciclo, contando `depends_on`, os vínculos de todos e os acessos ao banco compartilhado.

use crate::domain::{Service, ServiceSpec, is_valid_env_prefix};
use crate::protocol::Response as RpResponse;
use crate::store::Db;
use std::collections::BTreeSet;

/// O que a gravação precisa fazer além de salvar: os servidores compartilhados (de outros projetos)
/// que este projeto passa a usar e ainda não tem acesso.
#[derive(Debug, Default, PartialEq)]
pub struct LinkPlan {
    pub grant_servers: Vec<String>,
}

pub(crate) async fn check(
    db: &Db,
    spec: &ServiceSpec,
    self_id: Option<&str>,
) -> Result<LinkPlan, RpResponse> {
    if spec.links.is_empty() {
        return Ok(LinkPlan::default());
    }
    let all = crate::store::services::list_all(db)
        .await
        .map_err(|e| RpResponse::err("DatabaseError", e.to_string()))?;
    let access = crate::store::shared_access::all(db)
        .await
        .unwrap_or_default();
    let stored_links: BTreeSet<String> = self_id
        .and_then(|id| all.iter().find(|s| s.id == id))
        .map(|s| s.spec.links.iter().map(|l| l.service_id.clone()).collect())
        .unwrap_or_default();

    let plan = plan_links(spec, self_id, &all, &access, &stored_links)?;

    // Ciclo: o grafo inteiro com os vínculos NOVOS deste serviço no lugar dos gravados.
    if let Some(id) = self_id {
        let others: Vec<Service> = all.iter().filter(|s| s.id != id).cloned().collect();
        let mut implicit = crate::features::deploy::autostart::implicit_edges(&access, &others);
        for link in &spec.links {
            implicit.push((id.to_string(), link.service_id.clone()));
        }
        // As arestas implícitas de `shared_access` também partem deste serviço (ele é do projeto).
        for (server, project) in &access {
            if *project == spec.project_id && server != id {
                implicit.push((id.to_string(), server.clone()));
            }
        }
        let tuples: Vec<(String, String, Vec<String>)> = all
            .iter()
            .map(|s| (s.id.clone(), s.spec.name.clone(), s.spec.depends_on.clone()))
            .collect();
        crate::features::deploy::deps::shared_cycle(&tuples, &implicit, self_id, &spec.depends_on)
            .map_err(|nomes| {
                RpResponse::err(
                    "DependencyCycle",
                    format!(
                        "esse vínculo cria um ciclo (um serviço acabaria esperando a si mesmo): {}",
                        nomes.join(", ")
                    ),
                )
            })?;
    }
    Ok(plan)
}

/// As regras que não precisam de grafo (separadas para teste).
pub(crate) fn plan_links(
    spec: &ServiceSpec,
    self_id: Option<&str>,
    all: &[Service],
    access: &[(String, String)],
    stored_links: &BTreeSet<String>,
) -> Result<LinkPlan, RpResponse> {
    let mut seen = BTreeSet::new();
    let mut plan = LinkPlan::default();
    for link in &spec.links {
        if self_id == Some(link.service_id.as_str()) {
            return Err(RpResponse::err(
                "InvalidInput",
                "um serviço não pode se vincular a si mesmo",
            ));
        }
        if !seen.insert(link.service_id.as_str()) {
            return Err(RpResponse::err(
                "InvalidInput",
                "o mesmo serviço aparece duas vezes nos vínculos",
            ));
        }
        if let Some(prefix) = &link.env_prefix
            && !is_valid_env_prefix(prefix)
        {
            return Err(RpResponse::err(
                "InvalidInput",
                format!(
                    "prefixo de variável inválido: \"{prefix}\" (use só A-Z, 0-9 e _, sem começar por dígito)"
                ),
            ));
        }
        let Some(target) = all.iter().find(|s| s.id == link.service_id) else {
            return Err(RpResponse::err(
                "NotFound",
                format!("o serviço vinculado {} não existe", link.service_id),
            ));
        };
        if target.spec.project_id == spec.project_id {
            continue;
        }
        let Some(shared) = &target.spec.shared else {
            return Err(RpResponse::err(
                "LinkNotAllowed",
                format!(
                    "\"{}\" é de outro projeto: só dá para vincular a um serviço de outro projeto se ele \
                     for um banco compartilhado",
                    target.spec.name
                ),
            ));
        };
        let is_new = !stored_links.contains(&link.service_id);
        if is_new && !shared.accept_links {
            return Err(RpResponse::err(
                "LinkRefused",
                format!(
                    "o banco compartilhado \"{}\" está com a trava ligada: não aceita vínculos novos de \
                     outros projetos",
                    target.spec.name
                ),
            ));
        }
        let granted = access
            .iter()
            .any(|(server, project)| server == &target.id && project == &spec.project_id);
        if !granted {
            plan.grant_servers.push(target.id.clone());
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ServiceLink, SharedServerConfig};

    fn service(id: &str, project: &str, shared: Option<bool>) -> Service {
        let mut spec = crate::features::wizard::app_spec(id.into(), project.into());
        spec.name = id.into();
        spec.shared = shared.map(|accept_links| SharedServerConfig { accept_links });
        Service {
            id: id.into(),
            spec,
            status: crate::domain::ServiceStatus::Stopped,
            live_container_id: None,
            compose_project: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn with_links(project: &str, targets: &[&str]) -> ServiceSpec {
        let mut spec = crate::features::wizard::app_spec("api".into(), project.into());
        spec.links = targets
            .iter()
            .map(|t| ServiceLink {
                service_id: (*t).into(),
                env_prefix: None,
            })
            .collect();
        spec
    }

    fn code(r: Result<LinkPlan, RpResponse>) -> String {
        match r {
            Err(RpResponse::Err { code, .. }) => code,
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn regras_dos_vinculos() {
        let all = vec![
            service("db", "p1", None),
            service("outro_app", "p2", None),
            service("pg_aberto", "p2", Some(true)),
            service("pg_trancado", "p2", Some(false)),
        ];
        let none = BTreeSet::new();
        // mesmo projeto: livre, sem acesso a conceder
        assert_eq!(
            plan_links(&with_links("p1", &["db"]), Some("api"), &all, &[], &none).unwrap(),
            LinkPlan::default()
        );
        // a si mesmo, repetido, inexistente
        assert_eq!(
            code(plan_links(
                &with_links("p1", &["api"]),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "InvalidInput"
        );
        assert_eq!(
            code(plan_links(
                &with_links("p1", &["db", "db"]),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "InvalidInput"
        );
        assert_eq!(
            code(plan_links(
                &with_links("p1", &["sumiu"]),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "NotFound"
        );
        // outro projeto: só banco compartilhado
        assert_eq!(
            code(plan_links(
                &with_links("p1", &["outro_app"]),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "LinkNotAllowed"
        );
        // compartilhado aceitando, sem acesso ainda: concede
        assert_eq!(
            plan_links(
                &with_links("p1", &["pg_aberto"]),
                Some("api"),
                &all,
                &[],
                &none
            )
            .unwrap(),
            LinkPlan {
                grant_servers: vec!["pg_aberto".into()]
            }
        );
        // já com acesso: nada a conceder
        let access = vec![("pg_aberto".to_string(), "p1".to_string())];
        assert_eq!(
            plan_links(
                &with_links("p1", &["pg_aberto"]),
                Some("api"),
                &all,
                &access,
                &none
            )
            .unwrap(),
            LinkPlan::default()
        );
        // trava ligada: recusa o vínculo novo, mas mantém o que já existia
        assert_eq!(
            code(plan_links(
                &with_links("p1", &["pg_trancado"]),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "LinkRefused"
        );
        let stored: BTreeSet<String> = ["pg_trancado".to_string()].into();
        assert!(
            plan_links(
                &with_links("p1", &["pg_trancado"]),
                Some("api"),
                &all,
                &access,
                &stored
            )
            .is_ok()
        );
        // prefixo inválido
        let mut spec = with_links("p1", &["db"]);
        spec.links[0].env_prefix = Some("1abc".into());
        assert_eq!(
            code(plan_links(&spec, Some("api"), &all, &[], &none)),
            "InvalidInput"
        );
    }

    #[test]
    fn prefixo_padrao_e_validacao() {
        use crate::domain::{is_valid_env_prefix, link_env_prefix};
        assert_eq!(link_env_prefix("s3"), "S3");
        assert_eq!(link_env_prefix("pg-central"), "PG_CENTRAL");
        assert_eq!(link_env_prefix("landing page"), "LANDING_PAGE");
        assert_eq!(link_env_prefix("2fa"), "_2FA");
        assert!(is_valid_env_prefix("DB_MAIN"));
        assert!(!is_valid_env_prefix("db"));
        assert!(!is_valid_env_prefix("1DB"));
        assert!(!is_valid_env_prefix(""));
    }

    /// O vínculo vira aresta de dependência (a ordem do deploy em árvore e do boot).
    #[test]
    fn vinculo_vira_aresta() {
        let mut api = service("api", "p1", None);
        api.spec.links = vec![ServiceLink {
            service_id: "db".into(),
            env_prefix: None,
        }];
        let all = vec![service("db", "p1", None), api];
        let edges = crate::features::deploy::autostart::implicit_edges(&[], &all);
        assert_eq!(edges, vec![("api".to_string(), "db".to_string())]);
    }
}
