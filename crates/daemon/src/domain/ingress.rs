use serde::{Deserialize, Serialize};

/// One Docker container on the host (running or stopped), for the Docker tab's
/// Containers sub-tab. Host-wide (not just rustploy-managed) — `managed`/
/// `project`/`service` are best-effort attribution (label `rustploy.managed` +
/// the `rp_<safe_name>_...` container-name convention).
/// Uma rota de domínio viva na tabela do ingress proxy.
///
/// É o estado **em memória** do proxy, não o que está no banco: é isto que
/// decide para onde uma requisição HTTP vai agora. Quando um domínio responde
/// 502, a pergunta é se existe rota aqui e para onde ela aponta.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressDomainRoute {
    pub domain: String,
    /// `ip:porta` de cada réplica. Vazio = rota registrada sem backend (502).
    pub backends: Vec<String>,
    /// Serviço dono da rota; `rp-registry` para a rota interna do registry.
    pub service_id: String,
}

/// Uma rota de porta TCP viva (`host_port` → backends).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressPortRoute {
    pub host_port: u16,
    /// Vazio = listener no ar, porém sem destino (conexão recusada).
    pub backends: Vec<String>,
}

/// Foto da tabela de rotas do ingress (resposta de `Command::IngressRoutes`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct IngressSnapshot {
    pub domains: Vec<IngressDomainRoute>,
    pub ports: Vec<IngressPortRoute>,
}

#[cfg(test)]
mod ingress_tests {

    use crate::domain::*;

    fn spec_com(domains: Vec<DomainRoute>, port: u16, host_port: Option<u16>) -> ServiceSpec {
        ServiceSpec {
            name: "svc".into(),
            project_id: "prj".into(),
            source: ServiceSource::Compose(ComposeSource {
                content: String::new(),
                ingress_service: None,
            }),
            port,
            host_port,
            tcp_ports: vec![],
            domain: None,
            tls_enabled: false,
            env_vars: vec![],
            env_comments: vec![],
            volumes: vec![],
            healthcheck: Healthcheck::default(),
            replicas: 1,
            resources: ResourceLimits::default(),
            run_command: None,
            run_args: vec![],
            product: None,
            domains,
            pre_deploy_job_id: None,
            pre_deploy_job_ids: vec![],
            shared: None,
            depends_on: vec![],
            registry_auto_deploy: false,
            autostart: true,
        }
    }

    /// As portas que o ingress precisa alcançar saem dos domínios, não da
    /// `port` do serviço: é isto que faz um Supabase (serviço na 80, domínio
    /// na 8000) rotear para o gateway e não para outro container da stack.
    #[test]
    fn porta_do_dominio_vence_a_porta_do_servico() {
        let spec = spec_com(
            vec![DomainRoute {
                domain: "db.exemplo.com".into(),
                port: Some(8000),
                tls: true,
            }],
            80,
            None,
        );
        assert_eq!(spec.ingress_container_ports(), vec![8000]);
    }

    /// Domínio sem porta própria herda a `port` do serviço.
    #[test]
    fn dominio_sem_porta_usa_a_do_servico() {
        let spec = spec_com(
            vec![DomainRoute {
                domain: "a.exemplo.com".into(),
                port: None,
                tls: false,
            }],
            3000,
            None,
        );
        assert_eq!(spec.ingress_container_ports(), vec![3000]);
    }

    #[test]
    fn rota_de_porta_no_host_acrescenta_a_porta_do_servico() {
        let spec = spec_com(
            vec![DomainRoute {
                domain: "a.exemplo.com".into(),
                port: Some(8000),
                tls: false,
            }],
            5432,
            Some(15432),
        );
        assert_eq!(spec.ingress_container_ports(), vec![5432, 8000]);
    }

    /// Cada porta TCP leva o ingress a alcançar a porta do container dela (o console 9001 do RustFS).
    #[test]
    fn cada_porta_tcp_acrescenta_a_porta_do_container_dela() {
        let mut spec = spec_com(Vec::new(), 9000, None);
        spec.tcp_ports = vec![
            TcpPortRoute {
                host_port: 20002,
                container_port: None,
                label: None,
            },
            TcpPortRoute {
                host_port: 20003,
                container_port: Some(9001),
                label: Some("console".into()),
            },
        ];
        assert_eq!(spec.ingress_container_ports(), vec![9000, 9001]);
    }

    #[test]
    fn sem_dominio_e_sem_host_port_nao_ha_porta_a_alcancar() {
        assert!(
            spec_com(Vec::new(), 80, None)
                .ingress_container_ports()
                .is_empty()
        );
    }

    /// Specs gravados antes do campo existir continuam desserializando.
    #[test]
    fn compose_source_sem_ingress_service_e_retrocompativel() {
        let c: ComposeSource = serde_json::from_str(r#"{"content":"services: {}"}"#).unwrap();
        assert_eq!(c.ingress_service, None);
    }
}
