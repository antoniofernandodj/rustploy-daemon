//! Infra-as-Code: structs do manifesto declarativo (`rustploy.yml`).
//!
//! Estes tipos são *format-agnostic* (apenas `serde`); o parse de YAML vive nos
//! clientes. Eles mapeiam para os modelos internos [`Project`] e [`ServiceSpec`]
//! de forma ergonômica para edição humana e versionamento em Git.

use serde::{Deserialize, Serialize};
use shared::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ServiceManifest {
    pub name: String,
    pub source: SourceManifest,
    #[serde(default)]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    /// Portas TCP do host (várias, cada uma para uma porta do container — ver `ServiceSpec::tcp_ports`).
    /// Convive com o `host_port` legado, copiados como estão, como `domains` e `domain`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tcp_ports: Vec<TcpPortRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tls: bool,
    /// Rotas de domínio (várias, cada uma com porta/TLS próprios — ver
    /// `ServiceSpec::domains`). Convive com o `domain`/`tls` legado: os dois são
    /// copiados como estão, sem fundir, para o ida-e-volta ser exato.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<DomainRoute>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Comentários do editor `.env`, ancorados por chave (ver `EnvComment`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_comments: Vec<EnvComment>,
    /// Cada item: `host:container` ou `host:container:ro`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthcheckManifest>,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub replicas: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Produto gerenciado: postgres | mongodb | mariadb | mysql | redis | kafka | rabbitmq | nats |
    /// rustfs | minio. A chave se chamava `db:` (renomeada em 2026-11, junto com `ServiceSpec.product`);
    /// o alias deixa os manifestos antigos entrarem. TODO: tirar o alias na release seguinte.
    #[serde(default, alias = "db", skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    /// Servidor de banco compartilhado entre projetos (ver `ServiceSpec::shared`).
    /// Os projetos autorizados ficam no banco do daemon, não no manifesto.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,
}

/// Origem do serviço: exatamente uma das três chaves deve estar presente.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SourceManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<String>,
    /// Serviço, dentro do compose, que recebe o tráfego do ingress (ex.: `kong`).
    /// Sem isto o daemon descobre o alvo pela porta do domínio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_ingress: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GitManifest {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_stage: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub submodules: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
    /// Nome do Git provider conectado (Gitea) a usar para autenticar o clone —
    /// referência por **nome**, não pelo ID interno do banco (ver [`EnvDoc`]
    /// e [`GitProviderDoc`] para onde ficam os dados reais do provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthcheckManifest {
    /// `none` | `tcp` | `http` | `docker`
    #[serde(rename = "type", default = "default_hc_type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_period: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResourcesManifest {
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cpu_shares: u64,
    /// Aceita sufixos `k`/`m`/`g` (ex.: `256m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<String>,
}

// --------------------------------------------------------------------------
// Conversão manifesto -> modelos internos
// --------------------------------------------------------------------------

impl ServiceManifest {
    pub fn to_spec(
        &self,
        project_id: &str,
        provider_ids: &BTreeMap<String, String>,
    ) -> ServiceSpec {
        ServiceSpec {
            name: self.name.clone(),
            project_id: project_id.to_string(),
            source: self.source.to_source(provider_ids),
            port: self.port,
            host_port: self.host_port,
            tcp_ports: self.tcp_ports.clone(),
            domain: self.domain.clone(),
            tls_enabled: self.tls,
            env_vars: env_map_to_vars(&self.env),
            env_comments: self.env_comments.clone(),
            volumes: self
                .volumes
                .iter()
                .filter_map(|v| parse_volume(v))
                .collect(),
            healthcheck: self
                .healthcheck
                .as_ref()
                .map(HealthcheckManifest::to_healthcheck)
                .unwrap_or_default(),
            replicas: self.replicas.max(1),
            resources: self
                .resources
                .as_ref()
                .map(ResourcesManifest::to_limits)
                .unwrap_or_default(),
            run_command: self.command.clone(),
            run_args: self.args.clone(),
            product: self.product.clone(),
            domains: self.domains.clone(),
            // TODO(pre-deploy-gate): o manifesto ainda não expõe o pré-deploy
            // check; fica vazio no import/export.
            pre_deploy_job_id: None,
            pre_deploy_job_ids: vec![],
            depends_on: vec![],
            registry_auto_deploy: false,
            autostart: true,
            shared: self.shared.then(shared::SharedServerConfig::default),
        }
    }

    pub fn from_spec(svc: &Service, providers: &BTreeMap<String, GitProvider>) -> Self {
        let spec = &svc.spec;
        ServiceManifest {
            name: spec.name.clone(),
            source: SourceManifest::from_source(&spec.source, providers),
            port: spec.port,
            host_port: spec.host_port,
            tcp_ports: spec.tcp_ports.clone(),
            domain: spec.domain.clone(),
            tls: spec.tls_enabled,
            domains: spec.domains.clone(),
            env: env_vars_to_map(&spec.env_vars),
            env_comments: spec.env_comments.clone(),
            volumes: spec.volumes.iter().map(format_volume).collect(),
            healthcheck: HealthcheckManifest::from_healthcheck(&spec.healthcheck),
            replicas: spec.replicas.max(1),
            resources: ResourcesManifest::from_limits(&spec.resources),
            command: spec.run_command.clone(),
            args: spec.run_args.clone(),
            product: spec.product.clone(),
            shared: spec.shared.is_some(),
        }
    }
}

impl SourceManifest {
    fn to_source(&self, provider_ids: &BTreeMap<String, String>) -> ServiceSource {
        if let Some(image) = &self.registry {
            ServiceSource::Registry {
                image: image.clone(),
            }
        } else if let Some(g) = &self.git {
            let d = GitSource::default();
            ServiceSource::Git(GitSource {
                url: g.url.clone(),
                branch: g.branch.clone().unwrap_or(d.branch),
                root_path: g.root_path.clone().unwrap_or(d.root_path),
                watch_paths: g.watch_paths.clone(),
                submodules: g.submodules,
                dockerfile_path: g.dockerfile.clone().unwrap_or(d.dockerfile_path),
                build_context: g.context.clone().unwrap_or(d.build_context),
                build_stage: g.build_stage.clone(),
                credentials: g.credentials.clone(),
                username: g.username.clone(),
                provider_id: g
                    .provider
                    .as_ref()
                    .and_then(|name| provider_ids.get(name))
                    .cloned(),
            })
        } else if let Some(content) = &self.compose {
            ServiceSource::Compose(ComposeSource {
                content: content.clone(),
                ingress_service: self.compose_ingress.clone(),
            })
        } else {
            // Sem origem declarada: registry vazio (será rejeitado no deploy).
            ServiceSource::Registry {
                image: String::new(),
            }
        }
    }

    fn from_source(src: &ServiceSource, providers: &BTreeMap<String, GitProvider>) -> Self {
        match src {
            ServiceSource::Registry { image } => SourceManifest {
                registry: Some(image.clone()),
                ..Default::default()
            },
            ServiceSource::Git(g) => SourceManifest {
                git: Some(GitManifest {
                    url: g.url.clone(),
                    branch: Some(g.branch.clone()),
                    root_path: Some(g.root_path.clone()),
                    dockerfile: Some(g.dockerfile_path.clone()),
                    context: Some(g.build_context.clone()),
                    build_stage: g.build_stage.clone(),
                    submodules: g.submodules,
                    watch_paths: g.watch_paths.clone(),
                    username: g.username.clone(),
                    credentials: g.credentials.clone(),
                    provider: g
                        .provider_id
                        .as_ref()
                        .and_then(|id| providers.get(id))
                        .map(|p| p.name.clone()),
                }),
                ..Default::default()
            },
            ServiceSource::Archive(_) => SourceManifest {
                registry: Some(String::new()),
                ..Default::default()
            },
            ServiceSource::Compose(c) => SourceManifest {
                compose: Some(c.content.clone()),
                compose_ingress: c.ingress_service.clone(),
                ..Default::default()
            },
        }
    }
}

impl HealthcheckManifest {
    fn to_healthcheck(&self) -> Healthcheck {
        let d = Healthcheck::default();
        let kind = match self.kind.to_lowercase().as_str() {
            "none" => HealthcheckKind::None,
            "http" => HealthcheckKind::Http {
                path: self.path.clone().unwrap_or_else(|| "/".to_string()),
                expected_status: self.status.unwrap_or(200),
            },
            "docker" => HealthcheckKind::DockerNative,
            _ => HealthcheckKind::Tcp,
        };
        Healthcheck {
            kind,
            interval_secs: self.interval.unwrap_or(d.interval_secs),
            timeout_secs: self.timeout.unwrap_or(d.timeout_secs),
            retries: self.retries.unwrap_or(d.retries),
            start_period_secs: self.start_period.unwrap_or(d.start_period_secs),
        }
    }

    fn from_healthcheck(hc: &Healthcheck) -> Option<Self> {
        let (kind, path, status) = match &hc.kind {
            HealthcheckKind::None => ("none", None, None),
            HealthcheckKind::Tcp => ("tcp", None, None),
            HealthcheckKind::DockerNative => ("docker", None, None),
            HealthcheckKind::Http {
                path,
                expected_status,
            } => ("http", Some(path.clone()), Some(*expected_status)),
        };
        Some(HealthcheckManifest {
            kind: kind.to_string(),
            path,
            status,
            interval: Some(hc.interval_secs),
            timeout: Some(hc.timeout_secs),
            retries: Some(hc.retries),
            start_period: Some(hc.start_period_secs),
        })
    }
}

impl ResourcesManifest {
    fn to_limits(&self) -> ResourceLimits {
        ResourceLimits {
            cpu_shares: self.cpu_shares,
            mem_limit_bytes: self.mem.as_deref().and_then(parse_mem).unwrap_or(0),
        }
    }

    fn from_limits(limits: &ResourceLimits) -> Option<Self> {
        if limits.cpu_shares == 0 && limits.mem_limit_bytes == 0 {
            return None;
        }
        Some(ResourcesManifest {
            cpu_shares: limits.cpu_shares,
            mem: (limits.mem_limit_bytes > 0).then(|| humanize_mem(limits.mem_limit_bytes)),
        })
    }
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

pub const API_VERSION: &str = "rustploy/v1";

pub(crate) const SECRET_PREFIX: &str = "secret:";

pub(crate) fn env_map_to_vars(map: &BTreeMap<String, String>) -> Vec<EnvVar> {
    map.iter()
        .map(|(k, v)| EnvVar {
            key: k.clone(),
            value: match v.strip_prefix(SECRET_PREFIX) {
                Some(name) => EnvVarValue::Secret(name.to_string()),
                None => EnvVarValue::Plain(v.clone()),
            },
        })
        .collect()
}

pub(crate) fn env_vars_to_map(vars: &[EnvVar]) -> BTreeMap<String, String> {
    vars.iter()
        .map(|e| {
            let v = match &e.value {
                EnvVarValue::Plain(v) => v.clone(),
                EnvVarValue::Secret(name) => format!("{SECRET_PREFIX}{name}"),
            };
            (e.key.clone(), v)
        })
        .collect()
}

/// `host:container` ou `host:container:ro`.
fn parse_volume(s: &str) -> Option<VolumeMount> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }
    Some(VolumeMount {
        host_path: parts[0].to_string(),
        container_path: parts[1].to_string(),
        read_only: parts.get(2).map(|m| *m == "ro").unwrap_or(false),
    })
}

fn format_volume(v: &VolumeMount) -> String {
    if v.read_only {
        format!("{}:{}:ro", v.host_path, v.container_path)
    } else {
        format!("{}:{}", v.host_path, v.container_path)
    }
}

/// Aceita `1024`, `256k`, `256m`, `2g` (case-insensitive).
fn parse_mem(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()?.to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024u64),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    // `checked_mul`: o manifesto vem de fora, e `99999999999g` estouraria o u64.
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

fn humanize_mem(bytes: u64) -> String {
    const G: u64 = 1024 * 1024 * 1024;
    const M: u64 = 1024 * 1024;
    const K: u64 = 1024;
    if bytes.is_multiple_of(G) {
        format!("{}g", bytes / G)
    } else if bytes.is_multiple_of(M) {
        format!("{}m", bytes / M)
    } else if bytes.is_multiple_of(K) {
        format!("{}k", bytes / K)
    } else {
        bytes.to_string()
    }
}

fn default_hc_type() -> String {
    "tcp".to_string()
}
fn one() -> u32 {
    1
}
fn is_one(n: &u32) -> bool {
    *n == 1
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mem_nao_estoura_nem_entra_em_panico_com_entrada_ruim() {
        assert_eq!(parse_mem(""), None);
        assert_eq!(parse_mem("   "), None);
        assert_eq!(parse_mem("g"), None, "sem número");
        assert_eq!(parse_mem("12x"), None);
        assert_eq!(parse_mem("é"), None, "último char multibyte");
        assert_eq!(parse_mem("99999999999999999g"), None, "estouraria o u64");
        assert_eq!(parse_mem(" 2G "), Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn mem_round_trip() {
        assert_eq!(parse_mem("256m"), Some(256 * 1024 * 1024));
        assert_eq!(parse_mem("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_mem("1024"), Some(1024));
        assert_eq!(humanize_mem(256 * 1024 * 1024), "256m");
        assert_eq!(humanize_mem(2 * 1024 * 1024 * 1024), "2g");
    }
}
