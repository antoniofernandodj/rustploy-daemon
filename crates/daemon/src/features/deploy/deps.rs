//! Grafo de dependências entre serviços: ordenação em **ondas**, detecção de ciclo e validação.
//!
//! Lógica pura (sem banco nem Docker), para ser testada à vontade. Quem usa:
//! a validação em `features/services/{create,update,delete}.rs` e o orquestrador de subida
//! (`autostart.rs`). Plano: `docs/plano-backup-estado-em-git.md`, R8.

use std::collections::{BTreeMap, BTreeSet};

/// `id do serviço → ids dos quais ele depende`. `BTreeMap`/`BTreeSet` para a ordem ser estável.
pub type Graph = BTreeMap<String, BTreeSet<String>>;

/// Ordena em ondas: a onda 0 não depende de ninguém; a onda *n* só depende de ondas anteriores.
/// Dependências que **não são nós** do grafo são ignoradas (quem monta o grafo decide se isso é um
/// erro ou "serviço parado de propósito"). `Err(ids)` = os nós envolvidos em ciclo.
pub fn waves(graph: &Graph) -> Result<Vec<Vec<String>>, Vec<String>> {
    let mut pending: BTreeMap<&str, BTreeSet<&str>> = graph
        .iter()
        .map(|(id, deps)| {
            (
                id.as_str(),
                deps.iter()
                    .filter(|d| graph.contains_key(*d) && *d != id)
                    .map(String::as_str)
                    .collect(),
            )
        })
        .collect();
    // Auto-dependência é ciclo, mesmo sendo filtrada acima para não travar o laço.
    let self_loops: Vec<String> = graph
        .iter()
        .filter(|(id, deps)| deps.contains(*id))
        .map(|(id, _)| id.clone())
        .collect();
    if !self_loops.is_empty() {
        return Err(self_loops);
    }

    let mut out = Vec::new();
    while !pending.is_empty() {
        let ready: Vec<&str> = pending
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            return Err(pending.keys().map(|s| s.to_string()).collect());
        }
        for id in &ready {
            pending.remove(id);
        }
        for deps in pending.values_mut() {
            for id in &ready {
                deps.remove(id);
            }
        }
        out.push(ready.into_iter().map(String::from).collect());
    }
    Ok(out)
}

/// Quem depende (direta ou indiretamente) de `id`. Ainda sem uso fora dos testes: entra no aviso
/// de "parar um serviço do qual outros dependem" (Fase 3, UI).
#[allow(dead_code)]
pub fn dependents_transitive(graph: &Graph, id: &str) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut frontier = vec![id.to_string()];
    while let Some(cur) = frontier.pop() {
        for (node, deps) in graph {
            if deps.contains(&cur) && found.insert(node.clone()) {
                frontier.push(node.clone());
            }
        }
    }
    found
}

/// Erro de validação de `depends_on`: código estável + mensagem para a pessoa.
#[derive(Debug, PartialEq, Eq)]
pub struct DepError {
    pub code: &'static str,
    pub message: String,
}

/// Ciclo no grafo **completo**: o `depends_on` de todos os serviços (de todos os projetos) mais as arestas
/// implícitas do banco compartilhado. Devolve os nomes dos serviços envolvidos.
///
/// `validate` só enxerga o `depends_on` **do projeto**; um ciclo que passa por um banco compartilhado
/// (um serviço do projeto A espera o servidor do B, e o servidor do B espera algo do A) só aparece aqui,
/// e sem esta checagem travaria o deploy e o religamento do boot ("não tem ordem possível").
///
/// - `all`: `(id, nome, depends_on)` de **todos** os serviços;
/// - `implicit`: `(dependente, dependência)` derivadas de `shared_access`;
/// - `self_id`/`depends_on`: a mudança a validar (`self_id: None` = serviço novo).
pub fn shared_cycle(
    all: &[(String, String, Vec<String>)],
    implicit: &[(String, String)],
    self_id: Option<&str>,
    depends_on: &[String],
) -> Result<(), Vec<String>> {
    let ids: BTreeSet<&str> = all.iter().map(|(i, _, _)| i.as_str()).collect();
    let mut graph: Graph = all
        .iter()
        .map(|(id, _, deps)| {
            (
                id.clone(),
                deps.iter()
                    .filter(|d| ids.contains(d.as_str()))
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    let node = self_id.unwrap_or("\u{0}novo").to_string();
    graph.insert(
        node,
        depends_on
            .iter()
            .filter(|d| ids.contains(d.as_str()))
            .cloned()
            .collect(),
    );
    for (dependent, dependency) in implicit {
        if dependent != dependency
            && graph.contains_key(dependent)
            && graph.contains_key(dependency)
        {
            graph
                .entry(dependent.clone())
                .or_default()
                .insert(dependency.clone());
        }
    }
    waves(&graph).map(|_| ()).map_err(|cycle| {
        cycle
            .iter()
            .filter(|id| id.as_str() != "\u{0}novo")
            .map(|id| {
                all.iter()
                    .find(|(i, _, _)| i == id)
                    .map_or_else(|| id.clone(), |(_, n, _)| n.clone())
            })
            .collect()
    })
}

/// Valida o `depends_on` de um serviço (novo ou editado) contra os do projeto.
/// - `project_services`: `(id, nome, depends_on atual)` de **todos** os serviços do projeto;
/// - `self_id`: `None` na criação (o serviço ainda não tem id);
/// - `depends_on`: o que se quer gravar.
///
/// Recusa: dependência de si mesmo, de serviço que não existe **neste projeto**, repetida, e ciclo.
pub fn validate(
    project_services: &[(String, String, Vec<String>)],
    self_id: Option<&str>,
    depends_on: &[String],
) -> Result<(), DepError> {
    let name_of = |id: &str| {
        project_services
            .iter()
            .find(|(i, _, _)| i == id)
            .map(|(_, n, _)| n.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let mut seen = BTreeSet::new();
    for dep in depends_on {
        if Some(dep.as_str()) == self_id {
            return Err(DepError {
                code: "DependencyCycle",
                message: "um serviço não pode depender de si mesmo".into(),
            });
        }
        if !project_services.iter().any(|(id, _, _)| id == dep) {
            return Err(DepError {
                code: "InvalidDependency",
                message: format!(
                    "a dependência '{dep}' não existe neste projeto (só se depende de serviços do mesmo projeto)"
                ),
            });
        }
        if !seen.insert(dep) {
            return Err(DepError {
                code: "InvalidDependency",
                message: format!("a dependência '{}' está repetida", name_of(dep)),
            });
        }
    }

    // Grafo do projeto com a mudança aplicada. Na criação, usa um id provisório que ninguém
    // pode referenciar (já foi checado acima que toda dependência existe).
    let mut graph: Graph = project_services
        .iter()
        .map(|(id, _, deps)| (id.clone(), deps.iter().cloned().collect()))
        .collect();
    let node = self_id.unwrap_or("\u{0}novo").to_string();
    graph.insert(node, depends_on.iter().cloned().collect());

    if let Err(cycle) = waves(&graph) {
        let nomes: Vec<String> = cycle
            .iter()
            .filter(|id| id.as_str() != "\u{0}novo")
            .map(|id| name_of(id))
            .collect();
        return Err(DepError {
            code: "DependencyCycle",
            message: format!(
                "essa dependência cria um ciclo entre: {}",
                if nomes.is_empty() {
                    "(o serviço novo)".into()
                } else {
                    nomes.join(", ")
                }
            ),
        });
    }
    Ok(())
}

/// Serviços que dependem diretamente de `id` (para a guarda de exclusão). `(id, nome)`.
pub fn direct_dependents(
    project_services: &[(String, String, Vec<String>)],
    id: &str,
) -> Vec<(String, String)> {
    project_services
        .iter()
        .filter(|(_, _, deps)| deps.iter().any(|d| d == id))
        .map(|(i, n, _)| (i.clone(), n.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(edges: &[(&str, &[&str])]) -> Graph {
        edges
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn svcs(edges: &[(&str, &[&str])]) -> Vec<(String, String, Vec<String>)> {
        edges
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    format!("nome-{k}"),
                    v.iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn grafo_vazio_nao_tem_ondas() {
        assert_eq!(waves(&Graph::new()).unwrap(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn independentes_ficam_todos_na_primeira_onda() {
        let w = waves(&g(&[("a", &[]), ("b", &[]), ("c", &[])])).unwrap();
        assert_eq!(w, vec![vec!["a", "b", "c"]]);
    }

    #[test]
    fn banco_antes_da_api_antes_do_front() {
        let w = waves(&g(&[("front", &["api"]), ("api", &["db"]), ("db", &[])])).unwrap();
        assert_eq!(w, vec![vec!["db"], vec!["api"], vec!["front"]]);
    }

    #[test]
    fn diamante_agrupa_em_ondas() {
        let w = waves(&g(&[
            ("db", &[]),
            ("cache", &[]),
            ("api", &["db", "cache"]),
            ("worker", &["db"]),
            ("front", &["api"]),
        ]))
        .unwrap();
        assert_eq!(
            w,
            vec![vec!["cache", "db"], vec!["api", "worker"], vec!["front"]]
        );
    }

    #[test]
    fn dependencia_fora_do_grafo_e_ignorada() {
        let w = waves(&g(&[("api", &["parado"])])).unwrap();
        assert_eq!(w, vec![vec!["api"]]);
    }

    #[test]
    fn ciclo_direto_indireto_e_auto_dependencia() {
        assert!(waves(&g(&[("a", &["b"]), ("b", &["a"])])).is_err());
        let e = waves(&g(&[
            ("a", &["b"]),
            ("b", &["c"]),
            ("c", &["a"]),
            ("ok", &[]),
        ]))
        .unwrap_err();
        assert_eq!(e, vec!["a", "b", "c"], "só os do ciclo, não o inocente");
        assert!(waves(&g(&[("a", &["a"])])).is_err());
    }

    #[test]
    fn dependentes_transitivos() {
        let gr = g(&[
            ("db", &[]),
            ("api", &["db"]),
            ("front", &["api"]),
            ("outro", &[]),
        ]);
        let d = dependents_transitive(&gr, "db");
        assert_eq!(d.into_iter().collect::<Vec<_>>(), vec!["api", "front"]);
    }

    #[test]
    fn validate_aceita_dependencia_valida() {
        let s = svcs(&[("db", &[]), ("api", &[])]);
        assert!(validate(&s, Some("api"), &["db".into()]).is_ok());
        assert!(validate(&s, None, &["db".into(), "api".into()]).is_ok());
    }

    #[test]
    fn validate_recusa_inexistente_repetida_e_a_si_mesmo() {
        let s = svcs(&[("db", &[]), ("api", &[])]);
        assert_eq!(
            validate(&s, Some("api"), &["fantasma".into()])
                .unwrap_err()
                .code,
            "InvalidDependency"
        );
        assert_eq!(
            validate(&s, Some("api"), &["db".into(), "db".into()])
                .unwrap_err()
                .code,
            "InvalidDependency"
        );
        assert_eq!(
            validate(&s, Some("api"), &["api".into()]).unwrap_err().code,
            "DependencyCycle"
        );
    }

    #[test]
    fn validate_recusa_ciclo_criado_pela_edicao() {
        // db → api já existe; fazer api → db fecha o ciclo.
        let s = svcs(&[("db", &["api"]), ("api", &[])]);
        let e = validate(&s, Some("api"), &["db".into()]).unwrap_err();
        assert_eq!(e.code, "DependencyCycle");
        assert!(
            e.message.contains("nome-db") && e.message.contains("nome-api"),
            "{}",
            e.message
        );
    }

    #[test]
    fn serviço_novo_nao_cria_ciclo_sozinho() {
        let s = svcs(&[("db", &[])]);
        assert!(validate(&s, None, &["db".into()]).is_ok());
    }

    #[test]
    fn dependentes_diretos_para_a_guarda_de_exclusao() {
        let s = svcs(&[("db", &[]), ("api", &["db"]), ("job", &["db"]), ("x", &[])]);
        let d = direct_dependents(&s, "db");
        assert_eq!(d.len(), 2);
        assert!(direct_dependents(&s, "x").is_empty());
    }

    #[test]
    fn ciclo_pelo_banco_compartilhado_so_aparece_no_grafo_completo() {
        // p1: srv1; p2: srv2. Cada projeto está autorizado ao servidor do outro.
        let all = vec![
            ("srv1".to_string(), "srv1".to_string(), vec![]),
            ("srv2".to_string(), "srv2".to_string(), vec![]),
        ];
        let um_so = vec![("srv2".to_string(), "srv1".to_string())];
        assert!(shared_cycle(&all, &um_so, None, &[]).is_ok());
        let cruzado = vec![
            ("srv2".to_string(), "srv1".to_string()),
            ("srv1".to_string(), "srv2".to_string()),
        ];
        let nomes = shared_cycle(&all, &cruzado, None, &[]).unwrap_err();
        assert_eq!(nomes.len(), 2);
    }

    #[test]
    fn dependencia_explicita_que_fecha_ciclo_com_aresta_implicita() {
        // x depende (explícito) de srv; srv depende (implícito, por shared_access) de x.
        let all = vec![
            ("x".to_string(), "x".to_string(), vec![]),
            ("srv".to_string(), "srv".to_string(), vec![]),
        ];
        let implicito = vec![("srv".to_string(), "x".to_string())];
        assert!(shared_cycle(&all, &implicito, Some("x"), &["srv".to_string()]).is_err());
        assert!(shared_cycle(&all, &implicito, Some("x"), &[]).is_ok());
    }
}
