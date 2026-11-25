//! **Rota**: o que o proxy faz com um domínio, fora do serviço. Um domínio servido por vários
//! serviços (por prefixo de caminho), um domínio que só redireciona e uma senha na frente.
//! Os domínios de dentro do serviço (`ServiceSpec.domains`) continuam valendo; um domínio não pode
//! estar nos dois lugares. Ver `docs/arquivo/plano-releases-ambientes-rotas-volumes.md` §3.

use serde::{Deserialize, Serialize};

/// Uma rota, guardada no projeto (`ProjectInfra.routes`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    /// `rte_<ulid>`; vazio num `RouteSave` = criar.
    #[serde(default)]
    pub id: String,
    pub project_id: String,
    /// O host, sem esquema nem porta (`app.com`).
    pub domain: String,
    /// Certificado Let's Encrypt para o domínio.
    #[serde(default)]
    pub tls: bool,
    pub action: RouteAction,
    /// Usuário e senha pedidos antes de qualquer coisa (basic auth).
    #[serde(default)]
    pub basic_auth: Option<RouteBasicAuth>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RouteAction {
    /// Encaminha para serviços do projeto: vence a regra de **prefixo mais longo**.
    Forward { rules: Vec<PathRule> },
    /// Responde com um redirect para `to` + o caminho pedido (`www.app.com/x` → `https://app.com/x`).
    Redirect { to: String, permanent: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathRule {
    /// `/`, `/api`… Sempre começa com `/`.
    pub path_prefix: String,
    pub service_id: String,
    /// Porta do container; `None` = a porta padrão do serviço.
    #[serde(default)]
    pub port: Option<u16>,
    /// Tira o prefixo antes de mandar ao serviço (`/api/users` chega como `/users`).
    #[serde(default)]
    pub strip_prefix: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteBasicAuth {
    pub user: String,
    /// Na ida (cliente → daemon): a senha em claro, ou `None` para manter a gravada. Gravada cifrada
    /// (`ENC[age:…]`) e **nunca** devolvida: na volta vem sempre `None`.
    #[serde(default)]
    pub password: Option<String>,
}

/// O host normalizado (minúsculas, sem espaço, sem ponto final).
pub fn normalize_route_domain(domain: &str) -> String {
    domain.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Host aceito: letras, dígitos, `-` e `.`, com pelo menos um ponto ou `localhost`/`.test`.
pub fn is_valid_route_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        && !domain.starts_with('.')
        && !domain.starts_with('-')
        && (domain.contains('.') || domain == "localhost")
}

/// O prefixo casa com o caminho? `/api` casa com `/api` e `/api/x`, não com `/apix`; `/` casa com tudo.
pub fn prefix_matches(prefix: &str, path: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('?'))
}

/// A regra que atende o caminho: a de prefixo mais longo que casa.
pub fn match_rule<'a>(rules: &'a [PathRule], path: &str) -> Option<&'a PathRule> {
    rules
        .iter()
        .filter(|rule| prefix_matches(&rule.path_prefix, path))
        .max_by_key(|rule| rule.path_prefix.trim_end_matches('/').len())
}

/// O caminho que o serviço recebe com `strip_prefix` (sempre começa com `/`).
pub fn strip_path(prefix: &str, path_and_query: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let rest = path_and_query
        .strip_prefix(prefix)
        .unwrap_or(path_and_query);
    if rest.is_empty() {
        "/".to_string()
    } else if rest.starts_with('?') {
        format!("/{rest}")
    } else {
        rest.to_string()
    }
}

/// O `Location` de um redirect: o destino sem a barra final + o caminho pedido.
pub fn redirect_location(to: &str, path_and_query: &str) -> String {
    let base = to.trim_end_matches('/');
    if path_and_query == "/" {
        format!("{base}/")
    } else {
        format!("{base}{path_and_query}")
    }
}

/// Valida a forma de uma rota (o que não depende do resto do estado). Devolve a causa.
pub fn validate_route_shape(route: &Route) -> Result<(), String> {
    if !is_valid_route_domain(&route.domain) {
        return Err(format!("domínio inválido: {:?}", route.domain));
    }
    match &route.action {
        RouteAction::Forward { rules } => {
            if rules.is_empty() {
                return Err("a rota precisa de pelo menos um caminho".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            for rule in rules {
                if !rule.path_prefix.starts_with('/') {
                    return Err(format!(
                        "o caminho {:?} precisa começar com /",
                        rule.path_prefix
                    ));
                }
                if rule.path_prefix.contains(['?', '#', ' ']) {
                    return Err(format!("caminho inválido: {:?}", rule.path_prefix));
                }
                if !seen.insert(rule.path_prefix.trim_end_matches('/').to_string()) {
                    return Err(format!("caminho repetido: {:?}", rule.path_prefix));
                }
                if rule.service_id.is_empty() {
                    return Err(format!("o caminho {:?} não tem serviço", rule.path_prefix));
                }
            }
        }
        RouteAction::Redirect { to, .. } => {
            if !(to.starts_with("http://") || to.starts_with("https://")) || to.contains(' ') {
                return Err(format!(
                    "o destino do redirect precisa ser uma URL http(s): {to:?}"
                ));
            }
        }
    }
    if let Some(auth) = &route.basic_auth
        && (auth.user.is_empty() || auth.user.contains(':'))
    {
        return Err("usuário da senha inválido (vazio ou com ':')".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(prefix: &str, service: &str) -> PathRule {
        PathRule {
            path_prefix: prefix.into(),
            service_id: service.into(),
            port: None,
            strip_prefix: false,
        }
    }

    #[test]
    fn prefixo_mais_longo_vence_e_respeita_fronteira() {
        let rules = vec![
            rule("/", "front"),
            rule("/api", "api"),
            rule("/api/v2/", "v2"),
        ];
        let pick = |path: &str| match_rule(&rules, path).map(|r| r.service_id.as_str());
        assert_eq!(pick("/"), Some("front"));
        assert_eq!(pick("/apix"), Some("front"));
        assert_eq!(pick("/api"), Some("api"));
        assert_eq!(pick("/api/users"), Some("api"));
        assert_eq!(pick("/api?x=1"), Some("api"));
        assert_eq!(pick("/api/v2/x"), Some("v2"));
        let only_api = vec![rule("/api", "api")];
        assert!(match_rule(&only_api, "/other").is_none());
    }

    #[test]
    fn tirar_prefixo_e_redirect() {
        assert_eq!(strip_path("/api", "/api/users?x=1"), "/users?x=1");
        assert_eq!(strip_path("/api", "/api"), "/");
        assert_eq!(strip_path("/api/", "/api?x"), "/?x");
        assert_eq!(
            redirect_location("https://app.com/", "/x?y=1"),
            "https://app.com/x?y=1"
        );
        assert_eq!(
            redirect_location("https://app.com", "/"),
            "https://app.com/"
        );
    }

    #[test]
    fn validacao_da_forma() {
        let mut route = Route {
            id: String::new(),
            project_id: "p".into(),
            domain: "app.com".into(),
            tls: false,
            action: RouteAction::Forward {
                rules: vec![rule("/", "s")],
            },
            basic_auth: None,
        };
        assert!(validate_route_shape(&route).is_ok());
        route.domain = "App.com".into();
        assert!(validate_route_shape(&route).is_err());
        route.domain = normalize_route_domain(" App.com. ");
        assert_eq!(route.domain, "app.com");
        route.action = RouteAction::Forward {
            rules: vec![rule("/", "s"), rule("/", "t")],
        };
        assert!(validate_route_shape(&route).is_err());
        route.action = RouteAction::Forward {
            rules: vec![rule("api", "s")],
        };
        assert!(validate_route_shape(&route).is_err());
        route.action = RouteAction::Redirect {
            to: "app.com".into(),
            permanent: true,
        };
        assert!(validate_route_shape(&route).is_err());
        route.action = RouteAction::Redirect {
            to: "https://app.com".into(),
            permanent: true,
        };
        assert!(validate_route_shape(&route).is_ok());
    }
}
