use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::normalize_name;
use crate::domain::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceSpec {
    pub name: String,
    pub project_id: String,
    pub source: ServiceSource,
    pub port: u16,
    /// Legado (até 2026-11): **uma** porta TCP do host, sempre ligada à `port` do serviço. Substituído por
    /// `tcp_ports`; o daemon ainda o **espelha** (a rota que vai para a `port`) para clientes e daemons
    /// antigos não perderem a porta. Código novo lê [`ServiceSpec::tcp_routes`], nunca este campo.
    #[serde(default)]
    pub host_port: Option<u16>,
    /// Portas TCP do host ligadas a portas do container (passthrough no ingress, sem HTTP): cada uma pode ir
    /// para uma porta diferente do container (ex.: RustFS 9000 = API S3 e 9001 = console). Retrocompat: vazio
    /// cai no `host_port` legado. Ver [`ServiceSpec::tcp_routes`] e `docs/plano-multiplas-portas-tcp.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tcp_ports: Vec<TcpPortRoute>,
    pub domain: Option<String>,
    #[serde(default)]
    pub tls_enabled: bool,
    pub env_vars: Vec<EnvVar>,
    /// Comentários (`# ...`) do editor `.env`, ancorados por `key` (posição
    /// preservada mesmo que as vars sejam reordenadas). Não participa da
    /// construção do ambiente do container — só a lista/edição via UI olha
    /// pra isso.
    #[serde(default)]
    pub env_comments: Vec<EnvComment>,
    pub volumes: Vec<VolumeMount>,
    pub healthcheck: Healthcheck,
    pub replicas: u32,
    pub resources: ResourceLimits,
    #[serde(default)]
    pub run_command: Option<String>,
    #[serde(default)]
    pub run_args: Vec<String>,
    /// Produto de software gerenciado que roda no serviço — banco (postgres |
    /// mongodb | mariadb | mysql | redis), broker (kafka | rabbitmq | nats) ou
    /// storage (rustfs | minio); `None` = serviço comum. Controla a aba
    /// Connection no painel e a geração da internal connection URL (scheme por
    /// tipo: postgres://, amqp://, nats://, Kafka sem scheme). Antes se
    /// chamava `db_kind` (renomeado na 0.18.0).
    #[serde(default)]
    pub product: Option<String>,
    /// Rotas HTTP de domínio. Cada domínio pode apontar para uma porta de
    /// container diferente e ligar TLS de forma independente — permite um
    /// serviço que responde em várias portas expor vários subdomínios.
    /// Retrocompat: quando vazio, cai no `domain`/`tls_enabled` legado (roteado
    /// para `port`). Ver [`ServiceSpec::domain_routes`].
    #[serde(default)]
    pub domains: Vec<DomainRoute>,
    /// Legado (pré-fila, até 2026-08-05): um único `Job` de pré-deploy check.
    /// Substituído por `pre_deploy_job_ids`; mantido só pra retrocompat de
    /// specs salvos antes — nunca mais escrito por código novo. Ver
    /// [`ServiceSpec::pre_deploy_checks`].
    #[serde(default)]
    pub pre_deploy_job_id: Option<String>,
    /// Fila de `Job`s (Schedules) rodados em ORDEM antes de tocar em
    /// qualquer coisa (pull/build/staging) — vazio = sem checks, deploy
    /// segue direto. Cada item roda até completar (exit code do
    /// `main_service`); a primeira falha interrompe a fila **e** o deploy
    /// inteiro, sem rodar os checks seguintes. Ver `DeployState::
    /// PreDeployCheck` e `docs/plano-pre-deploy-gate.md`. Retrocompat: quando
    /// vazio, cai no `pre_deploy_job_id` legado — ver
    /// [`ServiceSpec::pre_deploy_checks`].
    #[serde(default)]
    pub pre_deploy_job_ids: Vec<String>,
    /// Se `Some`, este serviço de banco é um **servidor compartilhado**: o
    /// daemon o conecta às redes dos projetos autorizados (`shared_access`),
    /// sob o alias global [`shared_alias`](crate::domain::shared_alias). Só vale com
    /// `product` de banco. Ver `docs/plano-banco-compartilhado.md` §2.3/§4.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<SharedServerConfig>,
    /// IDs de serviços **do mesmo projeto** que precisam estar prontos antes deste subir (no boot
    /// do daemon e no "Subir projeto"). Sem ciclos — o daemon valida na criação/edição. Dependência
    /// de banco compartilhado **não** se declara aqui: é derivada de `shared_access`.
    /// Ver `docs/plano-backup-estado-em-git.md` (R8).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Participa do religamento automático (boot do daemon e "Subir projeto/tudo")? Padrão `true`.
    /// É *declaração* (vai para o YAML); a intenção momentânea "foi parado à mão" é outra coisa e
    /// mora no runtime do daemon (`desired_state`).
    #[serde(default = "default_autostart")]
    pub autostart: bool,
    /// Push-to-deploy: com `source = Registry` apontando para o registry **embutido**, cada
    /// `docker push` da tag configurada redeploya o serviço sozinho (sem webhook nem Git no
    /// servidor). Opt-in; padrão `false`. Ver `docs/plano-registry-embutido.md` (Fase 4).
    #[serde(default)]
    pub registry_auto_deploy: bool,
    /// **Vínculos**: os serviços que este **usa** (o banco, o S3, a fila). Cada vínculo conta como
    /// dependência (ordem no deploy e no boot) e, no deploy, entrega a este serviço as variáveis
    /// de conexão do outro (`<PREFIXO>_HOST`, `_PORT`, `_URL`) com a **menor** precedência: as
    /// variáveis digitadas no projeto ou no serviço sempre ganham. De outro projeto, só um banco
    /// compartilhado. Ver `docs/plano-recursos-e-vinculos.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ServiceLink>,
}

fn default_autostart() -> bool {
    true
}

/// "Este serviço usa aquele": ver [`ServiceSpec::links`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServiceLink {
    /// O serviço usado (o provedor).
    pub service_id: String,
    /// Prefixo das variáveis entregues (`S3` → `S3_HOST`…). `None` = o nome do provedor em
    /// maiúsculas ([`link_env_prefix`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_prefix: Option<String>,
    /// Um **recurso** que o provedor guarda para este serviço (um bucket, um database), criado no deploy
    /// se ainda não existir. Ver [`resource_kind`] e `docs/plano-recursos-e-vinculos.md` §7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<LinkResource>,
    /// O nome com que cada variável entra no container, pelo sufixo (`URL` → `S3_ENDPOINT`). Sufixo
    /// fora do mapa = o nome padrão (prefixo + sufixo); valor vazio = não entregar. Ver
    /// `docs/plano-nomes-das-variaveis-do-vinculo.md`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_names: BTreeMap<String, String>,
}

/// Os sufixos das variáveis que um vínculo pode entregar, na ordem em que aparecem na tela.
pub const LINK_ENV_SUFFIXES: [&str; 5] = ["HOST", "PORT", "URL", "BUCKET", "DATABASE"];

impl ServiceLink {
    /// O nome com que a variável `suffix` entra no container, ou `None` se não deve ser entregue.
    /// `default_prefix` é o prefixo padrão (o nome do provedor), usado quando não há `env_prefix`.
    pub fn env_name(&self, default_prefix: &str, suffix: &str) -> Option<String> {
        match self.env_names.get(suffix) {
            Some(name) if name.is_empty() => None,
            Some(name) => Some(name.clone()),
            None => {
                let prefix = self.env_prefix.as_deref().unwrap_or(default_prefix);
                Some(format!("{prefix}_{suffix}"))
            }
        }
    }
}

/// O recurso de um vínculo: o nome dele no provedor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LinkResource {
    pub name: String,
    /// Só bucket: deixa qualquer um **ler** os objetos (servir imagens, por exemplo).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub public_read: bool,
}

/// Que tipo de recurso um provedor oferece, pelo `product` dele.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// RustFS/MinIO: um bucket S3.
    Bucket,
    /// Postgres/MySQL/MariaDB: um database.
    Database,
}

pub fn resource_kind(product: Option<&str>) -> Option<ResourceKind> {
    match product?.to_ascii_lowercase().as_str() {
        "rustfs" | "minio" => Some(ResourceKind::Bucket),
        "postgres" | "postgresql" | "mysql" | "mariadb" => Some(ResourceKind::Database),
        _ => None,
    }
}

/// O nome do recurso é válido para o tipo? É a única barreira antes de ele entrar numa URL S3 ou num
/// SQL, então é estrita: bucket `[a-z0-9.-]`, 3–63, começa e termina com letra ou dígito; database
/// `[a-z][a-z0-9_]*`, até 32.
pub fn is_valid_resource_name(kind: ResourceKind, name: &str) -> bool {
    match kind {
        ResourceKind::Bucket => {
            (3..=63).contains(&name.len())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
                && name.starts_with(|c: char| c.is_ascii_alphanumeric())
                && name.ends_with(|c: char| c.is_ascii_alphanumeric())
                && !name.contains("..")
        }
        ResourceKind::Database => {
            !name.is_empty()
                && name.len() <= 32
                && name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
    }
}

/// O prefixo padrão das variáveis de um vínculo: o nome do provedor em maiúsculas, com tudo que
/// não for letra ou dígito virando `_` (`landing page` → `LANDING_PAGE`, `pg-central` → `PG_CENTRAL`).
pub fn link_env_prefix(provider_name: &str) -> String {
    let mut out: String = provider_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Um prefixo escrito pela pessoa é aceito? Só `A-Z`, `0-9` e `_`, sem começar por dígito.
pub fn is_valid_env_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && !prefix.starts_with(|c: char| c.is_ascii_digit())
        && prefix
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Configuração de um servidor de banco compartilhado. É struct (e não `bool`) para receber
/// opções sem quebrar o spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SharedServerConfig {
    /// A trava: aceita **vínculos novos** de serviços de outros projetos? Padrão `true` (um servidor
    /// compartilhado existe para ser usado). Desligada, os vínculos que já existem continuam.
    #[serde(default = "default_accept_links")]
    pub accept_links: bool,
}

impl Default for SharedServerConfig {
    fn default() -> Self {
        Self { accept_links: true }
    }
}

fn default_accept_links() -> bool {
    true
}

/// Uma rota HTTP de domínio de um serviço: qual domínio, para qual porta do
/// container e com ou sem TLS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DomainRoute {
    pub domain: String,
    /// Porta do container que este domínio atende. `None` = a `port` padrão do
    /// serviço.
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub tls: bool,
}

impl DomainRoute {
    /// Porta de container efetiva (a própria, ou a `port` padrão do serviço).
    pub fn container_port(&self, default: u16) -> u16 {
        self.port.unwrap_or(default)
    }
}

/// Uma porta do container que o serviço provavelmente tem, sugerida ao configurar as portas TCP.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PortSuggestion {
    /// Porta do container.
    pub port: u16,
    /// Nome conhecido ("API S3", "console", "painel"), se o produto do serviço o tiver na tabela.
    #[serde(default)]
    pub label: Option<String>,
    /// De onde veio, em ids estáveis para os clientes traduzirem: `"listening"` (o container escuta agora),
    /// `"image"` (a imagem/container declara), `"compose"` (o compose declara), `"product"` (tabela do
    /// produto). Uma porta pode vir de várias.
    pub sources: Vec<String>,
    /// Porta interna (ex.: cluster do RabbitMQ): existe, mas não deve ser exposta.
    #[serde(default)]
    pub internal: bool,
}

/// Máximo de portas TCP por serviço: uma lista longa por engano abriria meia faixa de portas no firewall.
pub const MAX_TCP_PORTS: usize = 8;

/// Uma porta TCP do host ligada a uma porta do container.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TcpPortRoute {
    /// Porta do host. `0` = "Automática": o daemon escolhe uma livre da faixa ao salvar.
    pub host_port: u16,
    /// Porta do container. `None` = a `port` padrão do serviço.
    #[serde(default)]
    pub container_port: Option<u16>,
    /// Nome para a tela ("console", "painel"). Não muda nada no roteamento.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl TcpPortRoute {
    /// Porta de container efetiva (a própria, ou a `port` padrão do serviço).
    pub fn container_port(&self, default: u16) -> u16 {
        self.container_port.unwrap_or(default)
    }
}

impl Service {
    /// Nome da stack Compose: o gravado; na falta (banco ainda não migrado, ou
    /// serviço que virou Compose depois), a fórmula legada com o nome atual —
    /// exatamente o que o daemon usava antes de o nome ser gravado.
    pub fn compose_project_name(&self) -> String {
        self.compose_project
            .clone()
            .unwrap_or_else(|| crate::domain::compose_project_name(&self.id, &self.spec.name))
    }
}

impl ServiceSpec {
    pub fn safe_name(&self) -> String {
        normalize_name(&self.name)
    }

    /// Rotas HTTP efetivas do serviço: a lista `domains` nova, ou — para specs
    /// antigos que só têm o campo `domain` — uma rota única sintetizada a partir
    /// do domínio legado (roteada para `port`, TLS = `tls_enabled`).
    pub fn domain_routes(&self) -> Vec<DomainRoute> {
        if !self.domains.is_empty() {
            self.domains.clone()
        } else if let Some(d) = &self.domain {
            vec![DomainRoute {
                domain: d.clone(),
                port: None,
                tls: self.tls_enabled,
            }]
        } else {
            Vec::new()
        }
    }

    /// Portas TCP efetivas do serviço: a lista `tcp_ports`, ou — para specs antigos que só têm o
    /// `host_port` — uma rota única para a `port` do serviço.
    pub fn tcp_routes(&self) -> Vec<TcpPortRoute> {
        if !self.tcp_ports.is_empty() {
            self.tcp_ports.clone()
        } else if let Some(p) = self.host_port {
            vec![TcpPortRoute {
                host_port: p,
                container_port: None,
                label: None,
            }]
        } else {
            Vec::new()
        }
    }

    /// Move o `host_port` legado para `tcp_ports` e **espelha** de volta no `host_port` a rota que vai para
    /// a `port` do serviço (a única que um cliente/daemon antigo entende: ele liga o `host_port` à `port`).
    /// Sem rota para a `port`, o espelho fica vazio, para nenhum antigo rotear para a porta errada.
    /// Idempotente; chamado antes de gravar o spec.
    pub fn materialize_tcp_ports(&mut self) {
        if self.tcp_ports.is_empty() {
            self.tcp_ports = self.tcp_routes();
        }
        let main = self.port;
        self.host_port = self
            .tcp_ports
            .iter()
            .find(|r| r.container_port(main) == main)
            .map(|r| r.host_port);
    }

    /// Portas de container que o ingress precisa alcançar: a de cada domínio e a de cada porta TCP do
    /// host. Ordenadas e sem repetição.
    ///
    /// Serve para escolher, numa stack Compose, qual container recebe a rota
    /// (ver `docker::containers::find_compose_ingress_container`).
    pub fn ingress_container_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self
            .domain_routes()
            .iter()
            .map(|r| r.container_port(self.port))
            .collect();
        ports.extend(
            self.tcp_routes()
                .iter()
                .map(|r| r.container_port(self.port)),
        );
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// Move o domínio legado (`domain`/`tls_enabled`) para a lista `domains` e
    /// zera os campos legados, para que edições subsequentes operem numa única
    /// fonte de verdade (a lista). Idempotente.
    pub fn materialize_domains(&mut self) {
        if self.domains.is_empty() {
            self.domains = self.domain_routes();
        }
        self.domain = None;
        self.tls_enabled = false;
    }

    /// Fila efetiva de checks de pré-deploy: `pre_deploy_job_ids` quando não
    /// vazia, ou — para specs antigos que só têm o campo singular — uma fila
    /// de um item sintetizada a partir do `pre_deploy_job_id` legado.
    pub fn pre_deploy_checks(&self) -> Vec<String> {
        if !self.pre_deploy_job_ids.is_empty() {
            self.pre_deploy_job_ids.clone()
        } else if let Some(j) = &self.pre_deploy_job_id {
            vec![j.clone()]
        } else {
            Vec::new()
        }
    }

    /// Move o `pre_deploy_job_id` legado para `pre_deploy_job_ids` e zera o
    /// campo legado — mesmo idioma de `materialize_domains`. Idempotente.
    pub fn materialize_pre_deploy_checks(&mut self) {
        if self.pre_deploy_job_ids.is_empty() {
            self.pre_deploy_job_ids = self.pre_deploy_checks();
        }
        self.pre_deploy_job_id = None;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Service {
    pub id: String,
    pub spec: ServiceSpec,
    /// Nome da stack Docker Compose (só serviços Compose), **gravado** na
    /// criação e nunca recalculado — renomear o serviço não muda a stack, então
    /// os volumes (`<stack>_<volume>`) continuam os mesmos. Só leitura para os
    /// clientes: vive numa coluna própria, fora do `spec`, que a GUI e a webui
    /// devolvem no `ServiceUpdate`. `None` em serviço que não é Compose; use
    /// [`Service::compose_project_name`].
    #[serde(default)]
    pub compose_project: Option<String>,
    pub status: ServiceStatus,
    pub live_container_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServiceStatus {
    Stopped,
    Stopping,
    Deploying,
    Running,
    Degraded,
    Error(String),
    /// Deploy criado e esperando na fila global — ainda não começou a rodar.
    Queued,
}

impl std::fmt::Display for ServiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "Stopped"),
            Self::Stopping => write!(f, "Stopping"),
            Self::Deploying => write!(f, "Deploying"),
            Self::Running => write!(f, "Running"),
            Self::Degraded => write!(f, "Degraded"),
            Self::Error(msg) => write!(f, "Error: {msg}"),
            Self::Queued => write!(f, "Queued"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VolumeMount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

/// `host_path` de um [`VolumeMount`] é o nome de um **volume nomeado** do motor (e não uma pasta do
/// servidor)? Nome = começa com letra ou dígito e só tem letras, dígitos, `_`, `.` e `-`.
pub fn is_named_volume(host_path: &str) -> bool {
    let mut chars = host_path.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && host_path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Valida uma montagem: o caminho no container é absoluto (e não `/`), e a origem é um volume
/// nomeado ou uma pasta absoluta do servidor. Devolve a causa.
pub fn validate_volume_mount(mount: &VolumeMount) -> Result<(), String> {
    let target = mount.container_path.trim_end_matches('/');
    if !mount.container_path.starts_with('/') || target.is_empty() {
        return Err(format!(
            "o caminho no container precisa ser absoluto e não pode ser /: {:?}",
            mount.container_path
        ));
    }
    if mount.container_path.split('/').any(|part| part == "..") {
        return Err(format!("caminho inválido: {:?}", mount.container_path));
    }
    if !(is_named_volume(&mount.host_path) || mount.host_path.starts_with('/')) {
        return Err(format!(
            "a origem precisa ser o nome de um volume (letras, dígitos, _ . -) ou uma pasta absoluta: {:?}",
            mount.host_path
        ));
    }
    Ok(())
}

#[cfg(test)]
mod volume_mount_tests {
    use super::*;

    fn mount(host: &str, target: &str) -> VolumeMount {
        VolumeMount {
            host_path: host.into(),
            container_path: target.into(),
            read_only: false,
        }
    }

    #[test]
    fn nome_de_volume_e_pasta() {
        assert!(is_named_volume("pgdata"));
        assert!(is_named_volume("rp_x_db.data-1"));
        assert!(!is_named_volume("/srv/data"));
        assert!(!is_named_volume("./data"));
        assert!(!is_named_volume(""));
        assert!(validate_volume_mount(&mount("pgdata", "/var/lib/postgresql/data")).is_ok());
        assert!(validate_volume_mount(&mount("/srv/uploads", "/data")).is_ok());
        assert!(validate_volume_mount(&mount("pgdata", "data")).is_err());
        assert!(validate_volume_mount(&mount("pgdata", "/")).is_err());
        assert!(validate_volume_mount(&mount("pgdata", "/a/../b")).is_err());
        assert!(validate_volume_mount(&mount("../x", "/data")).is_err());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Healthcheck {
    pub kind: HealthcheckKind,
    pub interval_secs: u32,
    pub timeout_secs: u32,
    pub retries: u32,
    pub start_period_secs: u32,
}

impl Default for Healthcheck {
    fn default() -> Self {
        Self {
            // `None` (não `Tcp`): serviço novo nasce SEM verificação. Era `Tcp`
            // aqui e `None` no caminho de criação da webui, então o mesmo
            // "criar serviço" dava healthchecks diferentes conforme o cliente e
            // o caminho (wizard × `ServiceCreate` direto). Escolhido `None`
            // porque a porta declarada nem sempre é uma porta que aceita
            // conexão — worker e job não escutam nada, e um `Tcp` padrão
            // reprovaria o deploy deles. O custo é o deploy passar mesmo com a
            // porta errada; por isso o log do deploy agora diz, em letras
            // claras, que nada foi testado (ver `deploy::executor`).
            kind: HealthcheckKind::None,
            interval_secs: 5,
            timeout_secs: 3,
            retries: 10,
            start_period_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HealthcheckKind {
    None,
    Http { path: String, expected_status: u16 },
    Tcp,
    DockerNative,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ResourceLimits {
    pub cpu_shares: u64,
    pub mem_limit_bytes: u64,
}

/// Se a imagem de um serviço com fonte Git ou zip já tem o cache de build quente
/// no host, de modo que o próximo deploy não baixe nem recompile tudo do zero.
/// Fonte única da verdade (`Command::ServiceWarmStatusList`): as telas só exibem
/// este valor, nunca o deduzem. Serviços de outras fontes (imagem pronta, etc.)
/// não têm build e não aparecem.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServiceWarmStatus {
    pub service_id: String,
    pub warm: bool,
    /// Há um aquecimento em andamento (ou esperando a vaga da fila) agora.
    pub warming: bool,
    /// Por que está frio (`warm == false`): nunca aquecido, imagem ausente,
    /// cache de build limpo, fonte alterada. `None` quando quente.
    pub reason: Option<String>,
    /// Quando o serviço foi aquecido pela última vez (aquecimento explícito ou
    /// deploy bem-sucedido).
    pub warmed_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod dependencias_tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            name: "api".into(),
            project_id: "p".into(),
            source: ServiceSource::Registry { image: "x".into() },
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
            resources: ResourceLimits::default(),
            run_command: None,
            run_args: vec![],
            product: None,
            domains: vec![],
            pre_deploy_job_id: None,
            pre_deploy_job_ids: vec![],
            shared: None,
            depends_on: vec!["svc_db".into()],
            registry_auto_deploy: false,
            autostart: false,
            links: Vec::new(),
        }
    }

    /// Specs gravados antes de existirem `depends_on`/`autostart` (JSON no SQLite) precisam
    /// continuar legíveis, com dependências vazias e autostart ligado.
    #[test]
    fn spec_antigo_sem_os_campos_novos_le_com_os_padroes() {
        let mut v = serde_json::to_value(spec()).unwrap();
        let obj = v.as_object_mut().unwrap();
        assert!(obj.remove("depends_on").is_some());
        assert!(obj.remove("autostart").is_some());
        let velho: ServiceSpec = serde_json::from_value(v).unwrap();
        assert!(velho.depends_on.is_empty());
        assert!(velho.autostart);
    }

    #[test]
    fn campos_novos_ida_e_volta() {
        let back: ServiceSpec =
            serde_json::from_str(&serde_json::to_string(&spec()).unwrap()).unwrap();
        assert_eq!(back.depends_on, vec!["svc_db".to_string()]);
        assert!(!back.autostart);
    }

    #[test]
    fn autostart_padrao_e_verdadeiro() {
        assert!(default_autostart());
    }
}

#[cfg(test)]
mod tcp_ports_tests {
    use super::*;

    fn spec(port: u16, host_port: Option<u16>) -> ServiceSpec {
        let mut v = serde_json::json!({
            "name": "s3", "project_id": "p", "source": {"Registry": {"image": "x"}},
            "port": port, "domain": null, "env_vars": [], "volumes": [],
            "healthcheck": Healthcheck::default(), "replicas": 1,
            "resources": ResourceLimits::default(),
        });
        v["host_port"] = serde_json::json!(host_port);
        serde_json::from_value(v).unwrap()
    }

    fn rota(host: u16, container: Option<u16>) -> TcpPortRoute {
        TcpPortRoute {
            host_port: host,
            container_port: container,
            label: None,
        }
    }

    /// Spec antigo (só `host_port`) continua significando a mesma coisa: uma porta para a `port`.
    #[test]
    fn host_port_legado_vira_uma_rota_para_a_porta_do_servico() {
        let s = spec(9000, Some(20002));
        assert!(
            s.tcp_ports.is_empty(),
            "o campo novo nasce vazio no spec antigo"
        );
        assert_eq!(s.tcp_routes(), vec![rota(20002, None)]);
        assert!(spec(9000, None).tcp_routes().is_empty());
    }

    #[test]
    fn materializar_move_o_legado_e_espelha_a_rota_da_porta_principal() {
        let mut s = spec(9000, Some(20002));
        s.materialize_tcp_ports();
        assert_eq!(s.tcp_ports, vec![rota(20002, None)]);
        assert_eq!(s.host_port, Some(20002));

        // a rota da porta principal pode ser a segunda: é ela que vai para o espelho
        s.tcp_ports = vec![rota(20003, Some(9001)), rota(20005, Some(9000))];
        s.materialize_tcp_ports();
        assert_eq!(s.host_port, Some(20005));
    }

    /// Um cliente/daemon antigo liga o `host_port` à `port`: espelhar uma rota de outra porta o faria
    /// rotear para o lugar errado.
    #[test]
    fn sem_rota_para_a_porta_principal_o_espelho_fica_vazio() {
        let mut s = spec(9000, None);
        s.tcp_ports = vec![rota(20003, Some(9001))];
        s.materialize_tcp_ports();
        assert_eq!(s.host_port, None);
        assert_eq!(s.tcp_routes(), vec![rota(20003, Some(9001))]);
    }

    /// Sem portas novas o spec serializa como antes (o YAML do git não muda para quem não usa).
    #[test]
    fn lista_vazia_nao_aparece_no_json() {
        let v = serde_json::to_value(spec(9000, Some(20002))).unwrap();
        assert!(v.get("tcp_ports").is_none());
    }
}
