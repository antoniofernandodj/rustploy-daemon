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

use crate::domain::{
    LINK_ENV_SUFFIXES, ResourceKind, Service, ServiceSpec, is_valid_env_prefix,
    is_valid_resource_name, link_env_prefix, resource_kind,
};
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
    // (nome entregue, foi renomeado?) de todos os vínculos, para achar nomes repetidos.
    let mut delivered: Vec<(String, bool)> = Vec::new();
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
        for (suffix, name) in &link.env_names {
            if !LINK_ENV_SUFFIXES.contains(&suffix.as_str()) {
                return Err(RpResponse::err(
                    "InvalidInput",
                    format!(
                        "variável de vínculo desconhecida: \"{suffix}\" (use {})",
                        LINK_ENV_SUFFIXES.join(", ")
                    ),
                ));
            }
            if !name.is_empty() && !is_valid_env_prefix(name) {
                return Err(RpResponse::err(
                    "InvalidInput",
                    format!(
                        "nome de variável inválido: \"{name}\" (use só A-Z, 0-9 e _, sem começar por dígito)"
                    ),
                ));
            }
        }
        let Some(target) = all.iter().find(|s| s.id == link.service_id) else {
            return Err(RpResponse::err(
                "NotFound",
                format!("o serviço vinculado {} não existe", link.service_id),
            ));
        };
        let default_prefix = link_env_prefix(&target.spec.name);
        let resource_suffix = match (
            &link.resource,
            resource_kind(target.spec.product.as_deref()),
        ) {
            (Some(_), Some(ResourceKind::Bucket)) => Some("BUCKET"),
            (Some(_), Some(ResourceKind::Database)) => Some("DATABASE"),
            _ => None,
        };
        let suffixes = ["HOST", "PORT", "URL"].into_iter().chain(resource_suffix);
        for suffix in suffixes {
            if let Some(name) = link.env_name(&default_prefix, suffix) {
                delivered.push((name, link.env_names.contains_key(suffix)));
            }
        }
        if let Some(resource) = &link.resource {
            if target.spec.project_id != spec.project_id {
                return Err(RpResponse::err(
                    "InvalidInput",
                    format!(
                        "recurso só em serviço do mesmo projeto; num banco compartilhado de outro \
                         projeto (\"{}\"), use um database gerenciado",
                        target.spec.name
                    ),
                ));
            }
            let Some(kind) = resource_kind(target.spec.product.as_deref()) else {
                return Err(RpResponse::err(
                    "InvalidInput",
                    format!(
                        "\"{}\" não oferece recursos (só S3 — RustFS, MinIO — e Postgres, MySQL, MariaDB)",
                        target.spec.name
                    ),
                ));
            };
            if !is_valid_resource_name(kind, &resource.name) {
                let rule = match kind {
                    ResourceKind::Bucket => {
                        "bucket: 3 a 63 caracteres, minúsculas, dígitos, ponto e hífen, começando e \
                         terminando com letra ou dígito"
                    }
                    ResourceKind::Database => {
                        "database: até 32 caracteres, minúsculas, dígitos e _, começando com letra"
                    }
                };
                return Err(RpResponse::err(
                    "InvalidInput",
                    format!("nome de recurso inválido \"{}\" ({rule})", resource.name),
                ));
            }
            if resource.public_read && kind != ResourceKind::Bucket {
                return Err(RpResponse::err(
                    "InvalidInput",
                    "leitura pública só vale para bucket",
                ));
            }
        }
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
    // Um nome renomeado não pode repetir outro nome entregue (um apagaria o outro). Só os renomeados:
    // dois vínculos antigos com o mesmo prefixo continuam gravando como antes.
    for (name, renamed) in &delivered {
        if *renamed && delivered.iter().filter(|(other, _)| other == name).count() > 1 {
            return Err(RpResponse::err(
                "InvalidInput",
                format!(
                    "a variável {name} seria entregue duas vezes pelos vínculos; escolha outro nome"
                ),
            ));
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
                resource: None,
                ..Default::default()
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

    /// Fase 2: recurso só no mesmo projeto, só em provedor que oferece, com nome válido; leitura
    /// pública só para bucket.
    #[test]
    fn regras_do_recurso() {
        use crate::domain::LinkResource;
        let mut s3 = service("s3", "p1", None);
        s3.spec.product = Some("rustfs".into());
        let mut pg = service("pg", "p1", None);
        pg.spec.product = Some("postgres".into());
        let mut pg_fora = service("pg_fora", "p2", Some(true));
        pg_fora.spec.product = Some("postgres".into());
        let all = vec![s3, pg, pg_fora, service("app", "p1", None)];
        let none = BTreeSet::new();
        let with = |target: &str, name: &str, public_read: bool| {
            let mut spec = with_links("p1", &[target]);
            spec.links[0].resource = Some(LinkResource {
                name: name.into(),
                public_read,
            });
            spec
        };
        assert!(
            plan_links(
                &with("s3", "chickie-uploads", true),
                Some("api"),
                &all,
                &[],
                &none
            )
            .is_ok()
        );
        assert!(plan_links(&with("pg", "loja", false), Some("api"), &all, &[], &none).is_ok());
        assert_eq!(
            code(plan_links(
                &with("pg", "loja", true),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "InvalidInput"
        );
        assert_eq!(
            code(plan_links(
                &with("s3", "Maiuscula", false),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "InvalidInput"
        );
        assert_eq!(
            code(plan_links(
                &with("pg", "1abc", false),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "InvalidInput"
        );
        assert_eq!(
            code(plan_links(
                &with("app", "x123", false),
                Some("api"),
                &all,
                &[],
                &none
            )),
            "InvalidInput"
        );
        let access = vec![("pg_fora".to_string(), "p1".to_string())];
        assert_eq!(
            code(plan_links(
                &with("pg_fora", "loja", false),
                Some("api"),
                &all,
                &access,
                &none
            )),
            "InvalidInput"
        );
    }

    #[test]
    fn nomes_de_recurso() {
        use crate::domain::{ResourceKind::*, is_valid_resource_name as ok};
        assert!(ok(Bucket, "chickie-uploads"));
        assert!(ok(Bucket, "a.b-c"));
        assert!(!ok(Bucket, "ab"));
        assert!(!ok(Bucket, "-ab"));
        assert!(!ok(Bucket, "a..b"));
        assert!(!ok(Bucket, "ABC"));
        assert!(ok(Database, "loja_2"));
        assert!(!ok(Database, "loja-2"));
        assert!(!ok(Database, "\"; DROP"));
        assert!(!ok(Database, &"a".repeat(33)));
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

    #[test]
    fn nomes_trocados_das_variaveis() {
        let all = vec![service("db", "p1", None), service("s3", "p1", None)];
        let none = BTreeSet::new();
        let names = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        // válido: renomear e não entregar
        let mut spec = with_links("p1", &["db"]);
        spec.links[0].env_names = names(&[("URL", "DATABASE_URL"), ("PORT", "")]);
        assert!(plan_links(&spec, Some("api"), &all, &[], &none).is_ok());
        // sufixo desconhecido e nome inválido
        spec.links[0].env_names = names(&[("SENHA", "X")]);
        assert_eq!(
            code(plan_links(&spec, Some("api"), &all, &[], &none)),
            "InvalidInput"
        );
        spec.links[0].env_names = names(&[("URL", "database_url")]);
        assert_eq!(
            code(plan_links(&spec, Some("api"), &all, &[], &none)),
            "InvalidInput"
        );
        // um nome trocado que repete outro nome entregue (S3_HOST do outro vínculo)
        let mut spec = with_links("p1", &["db", "s3"]);
        spec.links[0].env_names = names(&[("HOST", "S3_HOST")]);
        assert_eq!(
            code(plan_links(&spec, Some("api"), &all, &[], &none)),
            "InvalidInput"
        );
        // dois vínculos antigos com o mesmo prefixo (nada renomeado) continuam gravando
        let mut spec = with_links("p1", &["db", "s3"]);
        spec.links[0].env_prefix = Some("X".into());
        spec.links[1].env_prefix = Some("X".into());
        assert!(plan_links(&spec, Some("api"), &all, &[], &none).is_ok());
    }

    #[test]
    fn env_name_padrao_prefixo_e_troca() {
        let mut link = ServiceLink {
            service_id: "s3".into(),
            ..Default::default()
        };
        assert_eq!(link.env_name("S3", "URL").as_deref(), Some("S3_URL"));
        link.env_prefix = Some("STORAGE".into());
        assert_eq!(link.env_name("S3", "URL").as_deref(), Some("STORAGE_URL"));
        link.env_names.insert("URL".into(), "S3_ENDPOINT".into());
        link.env_names.insert("PORT".into(), String::new());
        assert_eq!(link.env_name("S3", "URL").as_deref(), Some("S3_ENDPOINT"));
        assert_eq!(link.env_name("S3", "PORT"), None);
        assert_eq!(link.env_name("S3", "HOST").as_deref(), Some("STORAGE_HOST"));
    }

    /// O vínculo vira aresta de dependência (a ordem do deploy em árvore e do boot).
    #[test]
    fn vinculo_vira_aresta() {
        let mut api = service("api", "p1", None);
        api.spec.links = vec![ServiceLink {
            service_id: "db".into(),
            env_prefix: None,
            resource: None,
            ..Default::default()
        }];
        let all = vec![service("db", "p1", None), api];
        let edges = crate::features::deploy::autostart::implicit_edges(&[], &all);
        assert_eq!(edges, vec![("api".to_string(), "db".to_string())]);
    }
}
