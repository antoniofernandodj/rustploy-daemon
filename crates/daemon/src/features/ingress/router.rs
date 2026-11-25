//! Tabela de rotas do ingress (domínio → backends, porta → backends) com
//! round-robin, lida sem lock pelo proxy.

use arc_swap::ArcSwap;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub domain: String,
    pub backends: Vec<String>,
    pub cursor: Arc<AtomicUsize>,
    pub service_id: String,
}

impl RouteEntry {
    pub fn next_backend(&self) -> Option<String> {
        if self.backends.is_empty() {
            return None;
        }
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.backends.len();
        Some(self.backends[idx].clone())
    }
}

#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    pub routes: HashMap<String, RouteEntry>,
    /// Rotas próprias (`domain::Route`), por domínio. Vêm antes das dos serviços.
    pub custom: HashMap<String, CustomRoute>,
    /// Os IPs no ar de cada serviço (e a porta padrão), atualizados a cada registro de rotas do
    /// serviço: é daqui que as rotas próprias tiram o destino na hora da requisição.
    pub services: HashMap<String, ServiceBackends>,
}

impl RouteTable {
    pub fn get(&self, domain: &str) -> Option<&RouteEntry> {
        self.routes.get(domain)
    }

    /// O que fazer com uma requisição para um domínio de rota própria. `authorization` é o
    /// cabeçalho recebido. `None` = o domínio não é de rota própria.
    pub fn decide_custom(
        &self,
        host: &str,
        path_and_query: &str,
        authorization: Option<&str>,
    ) -> Option<CustomDecision> {
        let route = self.custom.get(host)?;
        if let Some(expected) = &route.basic_auth
            && authorization != Some(expected.as_str())
        {
            return Some(CustomDecision::Unauthorized);
        }
        Some(match &route.action {
            CustomAction::Redirect { to, permanent } => CustomDecision::Redirect {
                location: crate::domain::redirect_location(to, path_and_query),
                permanent: *permanent,
            },
            CustomAction::Forward(rules) => {
                let path = path_and_query.split('?').next().unwrap_or("/");
                let Some(rule) = crate::domain::match_rule(rules, path) else {
                    return Some(CustomDecision::NotFound);
                };
                let backend = self.services.get(&rule.service_id).and_then(|service| {
                    if service.ips.is_empty() {
                        return None;
                    }
                    let index = route.cursor.fetch_add(1, Ordering::Relaxed) % service.ips.len();
                    let port = rule.port.unwrap_or(service.default_port);
                    Some(format!("{}:{port}", service.ips[index]))
                });
                CustomDecision::Forward {
                    backend,
                    path_and_query: rule
                        .strip_prefix
                        .then(|| crate::domain::strip_path(&rule.path_prefix, path_and_query)),
                }
            }
        })
    }
}

/// Os IPs no ar de um serviço e a porta padrão dele.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceBackends {
    pub ips: Vec<String>,
    pub default_port: u16,
}

#[derive(Debug, Clone)]
pub enum CustomAction {
    Forward(Vec<crate::domain::PathRule>),
    Redirect { to: String, permanent: bool },
}

/// Uma rota própria compilada para o proxy.
#[derive(Debug, Clone)]
pub struct CustomRoute {
    pub domain: String,
    /// A rota tem certificado: HTTP vira HTTPS. Sem TLS, ela é servida em HTTP mesmo.
    pub tls: bool,
    pub action: CustomAction,
    /// O cabeçalho `Authorization` esperado (`Basic <base64>`), se a rota pede senha.
    pub basic_auth: Option<String>,
    pub cursor: Arc<AtomicUsize>,
}

/// A decisão do proxy para uma requisição de rota própria.
#[derive(Debug, PartialEq)]
pub enum CustomDecision {
    Unauthorized,
    Redirect {
        location: String,
        permanent: bool,
    },
    /// Nenhum caminho da rota casa: 404.
    NotFound,
    /// `backend: None` = o serviço do caminho não está no ar (502). `path_and_query: Some` = o
    /// caminho reescrito (prefixo tirado).
    Forward {
        backend: Option<String>,
        path_and_query: Option<String>,
    },
}

/// Entrada nova para `domain`, reaproveitando o cursor de round-robin da
/// tabela atual para que a posição sobreviva a uma atualização de rota.
fn new_entry(
    table: &RouteTable,
    domain: &str,
    backends: Vec<String>,
    service_id: &str,
) -> RouteEntry {
    let cursor = table
        .get(domain)
        .map(|e| e.cursor.clone())
        .unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
    RouteEntry {
        domain: domain.to_string(),
        backends,
        cursor,
        service_id: service_id.to_string(),
    }
}

/// Shared handle to the live route table, readable lock-free from the proxy thread.
pub type RouteHandle = Arc<ArcSwap<RouteTable>>;

#[derive(Debug, Clone)]
pub struct PortBackends {
    pub addrs: Vec<String>,
    pub cursor: Arc<AtomicUsize>,
}

impl PortBackends {
    pub fn new(addrs: Vec<String>) -> Self {
        Self {
            addrs,
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn next(&self) -> Option<String> {
        if self.addrs.is_empty() {
            return None;
        }
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.addrs.len();
        Some(self.addrs[idx].clone())
    }
}

/// Backend(s) atual para um listener de porta específica. None = sem serviço ativo.
pub type PortBackend = Arc<ArcSwap<Option<PortBackends>>>;

#[derive(Clone)]
pub struct IngressController {
    table: RouteHandle,
    /// port → backend atual. Um listener tokio por porta já iniciada.
    port_backends: Arc<Mutex<HashMap<u16, PortBackend>>>,
}

impl IngressController {
    pub fn new() -> Self {
        Self {
            table: Arc::new(ArcSwap::from_pointee(RouteTable::default())),
            port_backends: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn upsert_route(&self, domain: &str, backends: Vec<String>, service_id: &str) {
        let old = self.table.load();
        let mut new_table = (**old).clone();
        let entry = new_entry(&new_table, domain, backends, service_id);
        new_table.routes.insert(domain.to_string(), entry);
        self.table.store(Arc::new(new_table));
    }

    pub fn remove_route(&self, domain: &str) {
        let old = self.table.load();
        let mut new_table = (**old).clone();
        new_table.routes.remove(domain);
        self.table.store(Arc::new(new_table));
    }

    /// Registra as rotas de domínio de um serviço e poda as órfãs dele.
    ///
    /// As rotas saem dos IPs dos containers live. Cada domínio é roteado para
    /// a sua porta de container (own `port`, ou a `port` padrão do serviço) —
    /// é isto que permite um
    /// serviço em várias portas expor um subdomínio por porta.
    ///
    /// As rotas deste `service_id` que **não** estão mais no spec são removidas
    /// no mesmo passo: um domínio tirado do serviço sobrevivia na tabela
    /// apontando para o IP de um container que já não existe, e respondia 502
    /// em vez de 404 — indistinguível, para quem olha, de um app quebrado.
    /// Como todo caminho que registra rota passa por aqui (deploy, rolling
    /// update, compose, reconcile de boot e sob demanda), a poda vale para
    /// todos eles.
    pub fn register_domains(
        &self,
        spec: &crate::domain::ServiceSpec,
        ips: &[String],
        service_id: &str,
    ) {
        let routes = spec.domain_routes();
        let old = self.table.load();
        let mut new_table = (**old).clone();

        new_table.routes.retain(|domain, entry| {
            entry.service_id != service_id || routes.iter().any(|r| &r.domain == domain)
        });

        for route in routes {
            let port = route.container_port(spec.port);
            let backends: Vec<String> = ips.iter().map(|ip| format!("{ip}:{port}")).collect();
            let entry = new_entry(&new_table, &route.domain, backends, service_id);
            new_table.routes.insert(route.domain, entry);
        }
        new_table.services.insert(
            service_id.to_string(),
            ServiceBackends {
                ips: ips.to_vec(),
                default_port: spec.port,
            },
        );

        self.table.store(Arc::new(new_table));
    }

    /// Esquece os IPs de um serviço (apagado ou sem container): as rotas próprias que apontam para
    /// ele passam a responder 502, como um domínio de serviço parado.
    pub fn forget_service(&self, service_id: &str) {
        let old = self.table.load();
        if !old.services.contains_key(service_id) {
            return;
        }
        let mut new_table = (**old).clone();
        new_table.services.remove(service_id);
        self.table.store(Arc::new(new_table));
    }

    /// Troca o conjunto inteiro de rotas próprias (cada gravação e o boot), mantendo o cursor de
    /// round-robin de quem continua.
    pub fn set_custom_routes(&self, routes: Vec<CustomRoute>) {
        let old = self.table.load();
        let mut new_table = (**old).clone();
        new_table.custom = routes
            .into_iter()
            .map(|mut route| {
                if let Some(previous) = old.custom.get(&route.domain) {
                    route.cursor = previous.cursor.clone();
                }
                (route.domain.clone(), route)
            })
            .collect();
        self.table.store(Arc::new(new_table));
    }

    /// Remove todas as rotas de domínio do serviço (parada/remoção/reconcile).
    pub fn remove_domains(&self, spec: &crate::domain::ServiceSpec) {
        for route in spec.domain_routes() {
            self.remove_route(&route.domain);
        }
    }

    pub fn _lookup(&self, domain: &str) -> Option<RouteEntry> {
        self.table.load().get(domain).cloned()
    }

    /// Foto da tabela de rotas viva, para `Command::IngressRoutes`.
    ///
    /// Ordenada (domínio, depois porta) porque isto é saída de diagnóstico
    /// lida por gente: a ordem de um `HashMap` mudaria a cada chamada.
    pub fn snapshot(&self) -> crate::domain::IngressSnapshot {
        let table = self.table.load();
        let mut domains: Vec<crate::domain::IngressDomainRoute> = table
            .routes
            .values()
            .map(|e| crate::domain::IngressDomainRoute {
                domain: e.domain.clone(),
                backends: e.backends.clone(),
                service_id: e.service_id.clone(),
            })
            .collect();
        // Rotas próprias: uma linha por caminho (`app.com/api`) ou o destino do redirect.
        for route in table.custom.values() {
            match &route.action {
                CustomAction::Forward(rules) => {
                    for rule in rules {
                        let backends = table
                            .services
                            .get(&rule.service_id)
                            .map(|service| {
                                let port = rule.port.unwrap_or(service.default_port);
                                service
                                    .ips
                                    .iter()
                                    .map(|ip| format!("{ip}:{port}"))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let suffix = if rule.path_prefix == "/" {
                            String::new()
                        } else {
                            rule.path_prefix.clone()
                        };
                        domains.push(crate::domain::IngressDomainRoute {
                            domain: format!("{}{suffix}", route.domain),
                            backends,
                            service_id: rule.service_id.clone(),
                        });
                    }
                }
                CustomAction::Redirect { to, .. } => {
                    domains.push(crate::domain::IngressDomainRoute {
                        domain: route.domain.clone(),
                        backends: vec![format!("↪ {to}")],
                        service_id: String::new(),
                    });
                }
            }
        }
        domains.sort_by(|a, b| a.domain.cmp(&b.domain));

        let mut ports: Vec<crate::domain::IngressPortRoute> = self
            .port_backends
            .lock()
            .unwrap()
            .iter()
            .map(|(port, backend)| crate::domain::IngressPortRoute {
                host_port: *port,
                // `None` = listener no ar sem destino; vira lista vazia.
                backends: (**backend.load())
                    .as_ref()
                    .map(|b| b.addrs.clone())
                    .unwrap_or_default(),
            })
            .collect();
        ports.sort_by_key(|p| p.host_port);

        crate::domain::IngressSnapshot { domains, ports }
    }

    pub fn table_handle(&self) -> RouteHandle {
        self.table.clone()
    }

    /// Aponta `host_port` para os `backends` fornecidos.
    /// Na primeira chamada para essa porta, sobe um listener TCP dedicado.
    pub fn upsert_port_route(&self, host_port: u16, backends: Vec<String>) {
        let mut ports = self.port_backends.lock().unwrap();
        if let Some(existing) = ports.get(&host_port) {
            // Reuse cursor to maintain round-robin continuity across redeploys
            let cursor = (**existing.load())
                .as_ref()
                .map(|b| b.cursor.clone())
                .unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
            existing.store(Arc::new(Some(PortBackends {
                addrs: backends,
                cursor,
            })));
        } else {
            let port_backend: PortBackend =
                Arc::new(ArcSwap::from_pointee(Some(PortBackends::new(backends))));
            ports.insert(host_port, port_backend.clone());
            tokio::spawn(crate::features::ingress::proxy::serve_port_proxy(
                host_port,
                port_backend,
            ));
        }
    }

    /// Remove o roteamento de `host_port` (conexões novas são recusadas com reset).
    pub fn remove_port_route(&self, host_port: u16) {
        if let Some(backend) = self.port_backends.lock().unwrap().get(&host_port) {
            backend.store(Arc::new(None));
        }
    }
}

impl Default for IngressController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DomainRoute, Healthcheck, ServiceSource, ServiceSpec};

    #[test]
    fn rota_propria_encaminha_redireciona_e_pede_senha() {
        let ing = IngressController::new();
        ing.register_domains(&spec(&[]), &["10.0.0.2".into()], "svc_front");
        let mut api = spec(&[]);
        api.port = 8080;
        ing.register_domains(&api, &["10.0.0.3".into()], "svc_api");
        let rule = |prefix: &str, service: &str, strip: bool| crate::domain::PathRule {
            path_prefix: prefix.into(),
            service_id: service.into(),
            port: None,
            strip_prefix: strip,
        };
        ing.set_custom_routes(vec![
            CustomRoute {
                domain: "app.com".into(),
                tls: false,
                action: CustomAction::Forward(vec![
                    rule("/", "svc_front", false),
                    rule("/api", "svc_api", true),
                    rule("/off", "svc_parado", false),
                ]),
                basic_auth: None,
                cursor: Arc::new(AtomicUsize::new(0)),
            },
            CustomRoute {
                domain: "www.app.com".into(),
                tls: false,
                action: CustomAction::Redirect {
                    to: "https://app.com".into(),
                    permanent: true,
                },
                basic_auth: None,
                cursor: Arc::new(AtomicUsize::new(0)),
            },
            CustomRoute {
                domain: "admin.app.com".into(),
                tls: false,
                action: CustomAction::Forward(vec![rule("/", "svc_front", false)]),
                basic_auth: Some("Basic YTpi".into()),
                cursor: Arc::new(AtomicUsize::new(0)),
            },
        ]);
        let table = ing.table_handle().load_full();
        assert_eq!(
            table.decide_custom("app.com", "/api/users?x=1", None),
            Some(CustomDecision::Forward {
                backend: Some("10.0.0.3:8080".into()),
                path_and_query: Some("/users?x=1".into()),
            })
        );
        assert_eq!(
            table.decide_custom("app.com", "/index.html", None),
            Some(CustomDecision::Forward {
                backend: Some("10.0.0.2:80".into()),
                path_and_query: None,
            })
        );
        assert_eq!(
            table.decide_custom("app.com", "/off", None),
            Some(CustomDecision::Forward {
                backend: None,
                path_and_query: None,
            })
        );
        assert_eq!(
            table.decide_custom("www.app.com", "/x", None),
            Some(CustomDecision::Redirect {
                location: "https://app.com/x".into(),
                permanent: true,
            })
        );
        assert_eq!(
            table.decide_custom("admin.app.com", "/", None),
            Some(CustomDecision::Unauthorized)
        );
        assert!(matches!(
            table.decide_custom("admin.app.com", "/", Some("Basic YTpi")),
            Some(CustomDecision::Forward { .. })
        ));
        assert_eq!(table.decide_custom("outro.com", "/", None), None);
        // A foto do Ingress mostra os caminhos.
        let snapshot = ing.snapshot();
        assert!(
            snapshot
                .domains
                .iter()
                .any(|d| d.domain == "app.com/api"
                    && d.backends == vec!["10.0.0.3:8080".to_string()])
        );
        ing.forget_service("svc_api");
        assert_eq!(
            ing.table_handle()
                .load()
                .decide_custom("app.com", "/api", None),
            Some(CustomDecision::Forward {
                backend: None,
                path_and_query: Some("/".into()),
            })
        );
    }

    fn spec(domains: &[&str]) -> ServiceSpec {
        ServiceSpec {
            description: None,
            name: "app".into(),
            project_id: "prj_1".into(),
            source: ServiceSource::Registry {
                image: "nginx".into(),
            },
            port: 80,
            host_port: None,
            tcp_ports: vec![],
            domain: None,
            tls_enabled: false,
            env_vars: vec![],
            env_comments: vec![],
            volumes: vec![],
            healthcheck: Healthcheck::default(),
            replicas: 1,
            resources: Default::default(),
            run_command: None,
            run_args: vec![],
            product: None,
            domains: domains
                .iter()
                .map(|d| DomainRoute {
                    domain: (*d).into(),
                    port: None,
                    tls: false,
                })
                .collect(),
            pre_deploy_job_id: None,
            pre_deploy_job_ids: vec![],
            depends_on: vec![],
            registry_auto_deploy: false,
            autostart: true,
            shared: None,
            links: Vec::new(),
        }
    }

    fn domains_of(ing: &IngressController) -> Vec<String> {
        ing.snapshot()
            .domains
            .into_iter()
            .map(|d| d.domain)
            .collect()
    }

    /// O caso que motivou a poda: o domínio sai do spec e a rota antiga fica
    /// apontando para o IP de um container morto, respondendo 502 em vez de 404.
    #[test]
    fn domain_removido_do_spec_perde_a_rota() {
        let ing = IngressController::new();
        ing.register_domains(
            &spec(&["itemize.com.br", "www.itemize.com.br"]),
            &["1.2.3.4".into()],
            "svc_1",
        );
        assert_eq!(
            domains_of(&ing),
            vec!["itemize.com.br", "www.itemize.com.br"]
        );

        ing.register_domains(&spec(&["www.itemize.com.br"]), &["1.2.3.5".into()], "svc_1");
        assert_eq!(domains_of(&ing), vec!["www.itemize.com.br"]);
        assert_eq!(
            ing.snapshot().domains[0].backends,
            vec!["1.2.3.5:80".to_string()]
        );
    }

    /// A poda é por serviço: o domínio de outro serviço não é tocado.
    #[test]
    fn poda_nao_alcanca_outro_servico() {
        let ing = IngressController::new();
        ing.register_domains(&spec(&["a.com"]), &["1.2.3.4".into()], "svc_1");
        ing.register_domains(&spec(&["b.com"]), &["1.2.3.5".into()], "svc_2");

        ing.register_domains(&spec(&["a.com"]), &["1.2.3.6".into()], "svc_1");
        assert_eq!(domains_of(&ing), vec!["a.com", "b.com"]);
    }

    /// Re-registrar não pode reiniciar o round-robin: senão todo deploy manda
    /// as primeiras requisições sempre para a mesma réplica.
    #[test]
    fn cursor_sobrevive_ao_re_registro() {
        let ing = IngressController::new();
        let s = spec(&["a.com"]);
        ing.register_domains(&s, &["1.1.1.1".into(), "2.2.2.2".into()], "svc_1");

        let table = ing.table.load();
        let entry = table.get("a.com").unwrap();
        assert_eq!(entry.next_backend().unwrap(), "1.1.1.1:80");
        drop(table);

        ing.register_domains(&s, &["1.1.1.1".into(), "2.2.2.2".into()], "svc_1");
        let table = ing.table.load();
        assert_eq!(
            table.get("a.com").unwrap().next_backend().unwrap(),
            "2.2.2.2:80"
        );
    }
}
