//! Lógica do wizard "Novo serviço" (Application / Database / Broker / Storage / Compose /
//! Template): catálogos, geração de senha, compose por banco/broker, e a
//! montagem do `ServiceSpec` final. O daemon a expõe por RPC
//! (`Command::WizardCatalog`/`WizardCreate`) — o cliente Luau (glacier-ui) só
//! dirige a UI e delega a construção do spec ao daemon, que tem os
//! blueprints de [`super::templates`]. O pedido (`WizardCreateReq`) vive em `shared`.

#![allow(dead_code)] // API de quando o módulo vivia no `shared`; parte dela só os testes usam.

use super::templates::{self, Template};
use serde_json::json;
use shared::{
    ComposeSource, EnvVar, EnvVarValue, Healthcheck, ResourceLimits, ServiceSource, ServiceSpec,
    WizardCreateReq,
};

// ── Bancos de dados ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    MongoDb,
    Postgres,
    MariaDb,
    MySql,
    Redis,
}

impl DbKind {
    pub const ALL: &'static [DbKind] = &[
        Self::MongoDb,
        Self::Postgres,
        Self::MariaDb,
        Self::MySql,
        Self::Redis,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::MongoDb => "MongoDB",
            Self::Postgres => "PostgreSQL",
            Self::MariaDb => "MariaDB",
            Self::MySql => "MySQL",
            Self::Redis => "Redis",
        }
    }

    pub fn default_image(self) -> &'static str {
        match self {
            Self::MongoDb => "mongo:8",
            Self::Postgres => "postgres:18",
            Self::MariaDb => "mariadb:11",
            Self::MySql => "mysql:8",
            Self::Redis => "redis:7",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::MongoDb => 27017,
            Self::Postgres => 5432,
            Self::MariaDb | Self::MySql => 3306,
            Self::Redis => 6379,
        }
    }

    pub fn kind_id(self) -> &'static str {
        match self {
            Self::MongoDb => "mongodb",
            Self::Postgres => "postgres",
            Self::MariaDb => "mariadb",
            Self::MySql => "mysql",
            Self::Redis => "redis",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "postgres" => Some(Self::Postgres),
            "mongodb" => Some(Self::MongoDb),
            "mariadb" => Some(Self::MariaDb),
            "mysql" => Some(Self::MySql),
            "redis" => Some(Self::Redis),
            _ => None,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::MongoDb => "root",
            Self::MariaDb | Self::MySql => "user",
            Self::Redis => "",
        }
    }

    pub fn has_db_name(self) -> bool {
        matches!(self, Self::Postgres | Self::MariaDb | Self::MySql)
    }
    pub fn has_user(self) -> bool {
        !matches!(self, Self::Redis)
    }
    pub fn has_root_password(self) -> bool {
        matches!(self, Self::MariaDb | Self::MySql)
    }
    pub fn has_replica_sets(self) -> bool {
        matches!(self, Self::MongoDb)
    }
}

// ── Message brokers ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerKind {
    Kafka,
    RabbitMq,
    Nats,
}

impl BrokerKind {
    pub const ALL: &'static [BrokerKind] = &[Self::Kafka, Self::RabbitMq, Self::Nats];

    pub fn label(self) -> &'static str {
        match self {
            Self::Kafka => "Apache Kafka",
            Self::RabbitMq => "RabbitMQ",
            Self::Nats => "NATS",
        }
    }

    pub fn default_image(self) -> &'static str {
        match self {
            Self::Kafka => "apache/kafka:3.9.0",
            Self::RabbitMq => "rabbitmq:4-management",
            Self::Nats => "nats:2",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::Kafka => 9092,
            Self::RabbitMq => 5672,
            Self::Nats => 4222,
        }
    }

    pub fn kind_id(self) -> &'static str {
        match self {
            Self::Kafka => "kafka",
            Self::RabbitMq => "rabbitmq",
            Self::Nats => "nats",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "kafka" => Some(Self::Kafka),
            "rabbitmq" => Some(Self::RabbitMq),
            "nats" => Some(Self::Nats),
            _ => None,
        }
    }

    pub fn has_user(self) -> bool {
        matches!(self, Self::RabbitMq)
    }

    pub fn default_user(self) -> &'static str {
        match self {
            Self::RabbitMq => "rabbit",
            _ => "",
        }
    }
}

/// Senha aleatória alfanumérica (URL-safe) de `n` caracteres, vinda de `/dev/urandom`.
///
/// Amostragem por rejeição: `256 = 4 × 62 + 8`, então bytes `>= 248` são descartados e cada
/// caractere do alfabeto sai com a mesma probabilidade (um `% 62` direto favoreceria os 8
/// primeiros). Antes era um splitmix64 semeado no relógio, que não é aleatório de verdade — e a
/// GUI ainda gerava a senha por conta própria com `math.random(os.time())`; agora quem gera é só
/// o daemon (ver [`completar_senhas`]).
pub fn generate_password(n: usize) -> Result<String, String> {
    use std::io::Read;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut urandom = std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("sem fonte de aleatoriedade (/dev/urandom): {e}"))?;
    let mut out = String::with_capacity(n);
    let mut buf = [0u8; 64];
    while out.len() < n {
        urandom
            .read_exact(&mut buf)
            .map_err(|e| format!("falha ao ler /dev/urandom: {e}"))?;
        for &b in &buf {
            if b < 248 && out.len() < n {
                out.push(ALPHABET[(b % 62) as usize] as char);
            }
        }
    }
    Ok(out)
}

/// Tamanho das senhas geradas pelo wizard.
const PASSWORD_LEN: usize = 24;

/// Banco/broker novo nasce com senha forte mesmo que a pessoa não digite nada: campo vazio =
/// "gere para mim". Senha digitada é respeitada. `precisa_root` só vale para MariaDB/MySQL.
fn completar_senhas(f: &mut DbFormInput, precisa: bool, precisa_root: bool) -> Result<(), String> {
    if precisa && f.password.trim().is_empty() {
        f.password = generate_password(PASSWORD_LEN)?;
    }
    if precisa_root && f.root_password.trim().is_empty() {
        f.root_password = generate_password(PASSWORD_LEN)?;
    }
    Ok(())
}

// ── Catálogos (JSON iterado pelos ForEach do wizard) ────────────────────────

/// Passo "escolha o banco": inclui os flags de formulário (has_*) e o user
/// default para o cliente pré-preencher a tela do banco escolhido.
pub fn db_rows_json() -> String {
    let rows: Vec<serde_json::Value> = DbKind::ALL
        .iter()
        .map(|d| {
            json!({
                "id": d.kind_id(),
                "label": d.label(),
                "image": d.default_image(),
                "user": d.default_user(),
                "has_db_name": d.has_db_name(),
                "has_user": d.has_user(),
                "has_rootpw": d.has_root_password(),
                "has_replica": d.has_replica_sets(),
            })
        })
        .collect();
    serde_json::Value::Array(rows).to_string()
}

/// Passo "escolha o broker".
pub fn broker_rows_json() -> String {
    let rows: Vec<serde_json::Value> = BrokerKind::ALL
        .iter()
        .map(|b| {
            json!({
                "id": b.kind_id(),
                "label": b.label(),
                "image": b.default_image(),
                "user": b.default_user(),
                "has_user": b.has_user(),
            })
        })
        .collect();
    serde_json::Value::Array(rows).to_string()
}

/// Templates filtrados por `search`, com as variáveis editáveis embutidas em
/// cada linha (`vars: [{idx,label,placeholder}]`) e o `slug` default — assim o
/// cliente pré-preenche o passo de configuração sem outro round-trip.
///
/// `logo` é o caminho `assets/blueprint-logos/<id>/<arquivo>` — a convenção do
/// cliente `rustploy-gui`, que é quem guarda as imagens (elas não vivem neste
/// crate); só carrega quando o cliente roda com esse working-dir/embutido.
pub fn templates_catalog_json(search: &str) -> String {
    let rows: Vec<serde_json::Value> = templates::filtered(search)
        .into_iter()
        .map(|t| {
            let (logo, logo_kind) = template_logo(t);
            let vars: Vec<serde_json::Value> = templates::editable_vars(t)
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let label = if v.key == "main_domain" {
                        "Domínio".to_string()
                    } else {
                        v.key.replace('_', " ")
                    };
                    json!({ "idx": i.to_string(), "label": label, "placeholder": "meuapp.exemplo.com" })
                })
                .collect();
            json!({
                "id": t.id,
                "name": t.name,
                "description": t.description,
                "logo": logo,
                "logo_kind": logo_kind,
                "slug": template_slug(t),
                "vars": vars,
            })
        })
        .collect();
    serde_json::Value::Array(rows).to_string()
}

fn template_logo(t: &'static Template) -> (String, &'static str) {
    if t.logo.is_empty() {
        return (String::new(), "none");
    }
    let ext = t.logo.rsplit('.').next().unwrap_or("").to_lowercase();
    let kind = match ext.as_str() {
        "svg" => "svg",
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico" => "img",
        _ => "none",
    };
    let path = format!("assets/blueprint-logos/{}/{}", t.id, t.logo);
    (path, kind)
}

pub fn template_slug(t: &Template) -> String {
    t.name.to_lowercase().replace(' ', "-")
}

// ── Montagem do ServiceSpec ──────────────────────────────────────────────────

/// Campos do formulário de banco/broker.
#[derive(Default)]
pub struct DbFormInput {
    pub db_name: String,
    pub user: String,
    pub password: String,
    pub root_password: String,
    pub image: String,
    pub use_replica_sets: bool,
}

fn base_spec(
    name: String,
    project_id: String,
    source: ServiceSource,
    port: u16,
    env_vars: Vec<EnvVar>,
    product: Option<String>,
) -> ServiceSpec {
    ServiceSpec {
        name,
        project_id,
        source,
        port,
        host_port: None,
        domain: None,
        tls_enabled: false,
        env_vars,
        env_comments: vec![],
        volumes: vec![],
        healthcheck: Healthcheck::default(),
        replicas: 1,
        resources: ResourceLimits::default(),
        run_command: None,
        run_args: vec![],
        product,
        domains: vec![],
        pre_deploy_job_id: None,
        pre_deploy_job_ids: vec![],
        depends_on: vec![],
        autostart: true,
        shared: None,
    }
}

pub fn app_spec(name: String, project_id: String) -> ServiceSpec {
    base_spec(
        name,
        project_id,
        ServiceSource::Registry {
            image: String::new(),
        },
        80,
        vec![],
        None,
    )
}

pub fn compose_spec(name: String, project_id: String) -> ServiceSpec {
    base_spec(
        name,
        project_id,
        ServiceSource::Compose(ComposeSource {
            content: String::new(),
            ingress_service: None,
        }),
        80,
        vec![],
        None,
    )
}

pub fn db_spec(db: DbKind, name: String, project_id: String, f: &DbFormInput) -> ServiceSpec {
    let image = if f.image.trim().is_empty() {
        db.default_image().to_string()
    } else {
        f.image.trim().to_string()
    };
    let svc = format!("rp_{}", shared::normalize_name(&name));
    base_spec(
        name,
        project_id,
        ServiceSource::Compose(ComposeSource {
            content: db_compose(db, &svc, &image, f),
            ingress_service: None,
        }),
        db.default_port(),
        db_env_vars(db, f),
        Some(db.kind_id().to_string()),
    )
}

fn db_env_vars(db: DbKind, f: &DbFormInput) -> Vec<EnvVar> {
    let plain = |k: &str, v: &str| EnvVar {
        key: k.to_string(),
        value: EnvVarValue::Plain(v.to_string()),
    };
    match db {
        DbKind::Postgres => vec![
            plain("POSTGRES_DB", &f.db_name),
            plain("POSTGRES_USER", &f.user),
            plain("POSTGRES_PASSWORD", &f.password),
        ],
        DbKind::MongoDb => {
            let mut vars = vec![
                plain("MONGO_INITDB_ROOT_USERNAME", &f.user),
                plain("MONGO_INITDB_ROOT_PASSWORD", &f.password),
            ];
            if f.use_replica_sets {
                vars.push(plain("MONGO_REPLICA_SET_NAME", "rs0"));
            }
            vars
        }
        DbKind::MariaDb | DbKind::MySql => vec![
            plain("MYSQL_DATABASE", &f.db_name),
            plain("MYSQL_USER", &f.user),
            plain("MYSQL_PASSWORD", &f.password),
            plain("MYSQL_ROOT_PASSWORD", &f.root_password),
        ],
        DbKind::Redis => {
            if f.password.is_empty() {
                vec![]
            } else {
                vec![plain("REDIS_PASSWORD", &f.password)]
            }
        }
    }
}

fn db_compose(db: DbKind, svc: &str, img: &str, f: &DbFormInput) -> String {
    match db {
        DbKind::Postgres => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      POSTGRES_DB: ${{POSTGRES_DB}}\n      POSTGRES_USER: ${{POSTGRES_USER}}\n      POSTGRES_PASSWORD: ${{POSTGRES_PASSWORD}}\n    volumes:\n      - pgdata:/var/lib/postgresql\n\nvolumes:\n  pgdata:\n"
        ),
        DbKind::MongoDb => {
            let replica = if f.use_replica_sets {
                "      MONGO_REPLICA_SET_NAME: rs0\n"
            } else {
                ""
            };
            format!(
                "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      MONGO_INITDB_ROOT_USERNAME: ${{MONGO_INITDB_ROOT_USERNAME}}\n      MONGO_INITDB_ROOT_PASSWORD: ${{MONGO_INITDB_ROOT_PASSWORD}}\n{replica}    volumes:\n      - mongodata:/data/db\n\nvolumes:\n  mongodata:\n"
            )
        }
        DbKind::MariaDb => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      MYSQL_DATABASE: ${{MYSQL_DATABASE}}\n      MYSQL_USER: ${{MYSQL_USER}}\n      MYSQL_PASSWORD: ${{MYSQL_PASSWORD}}\n      MYSQL_ROOT_PASSWORD: ${{MYSQL_ROOT_PASSWORD}}\n    volumes:\n      - mariadbdata:/var/lib/mysql\n\nvolumes:\n  mariadbdata:\n"
        ),
        DbKind::MySql => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      MYSQL_DATABASE: ${{MYSQL_DATABASE}}\n      MYSQL_USER: ${{MYSQL_USER}}\n      MYSQL_PASSWORD: ${{MYSQL_PASSWORD}}\n      MYSQL_ROOT_PASSWORD: ${{MYSQL_ROOT_PASSWORD}}\n    volumes:\n      - mysqldata:/var/lib/mysql\n\nvolumes:\n  mysqldata:\n"
        ),
        DbKind::Redis => {
            let cmd = if f.password.is_empty() {
                String::new()
            } else {
                "    command: redis-server --requirepass ${REDIS_PASSWORD}\n".to_string()
            };
            format!(
                "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n{cmd}    volumes:\n      - redisdata:/data\n\nvolumes:\n  redisdata:\n"
            )
        }
    }
}

pub fn broker_spec(
    broker: BrokerKind,
    name: String,
    project_id: String,
    f: &DbFormInput,
) -> ServiceSpec {
    let image = if f.image.trim().is_empty() {
        broker.default_image().to_string()
    } else {
        f.image.trim().to_string()
    };
    let svc = format!("rp_{}", shared::normalize_name(&name));
    base_spec(
        name,
        project_id,
        ServiceSource::Compose(ComposeSource {
            content: broker_compose(broker, &svc, &image, f),
            ingress_service: None,
        }),
        broker.default_port(),
        broker_env_vars(broker, f),
        Some(broker.kind_id().to_string()),
    )
}

fn broker_env_vars(broker: BrokerKind, f: &DbFormInput) -> Vec<EnvVar> {
    let plain = |k: &str, v: &str| EnvVar {
        key: k.to_string(),
        value: EnvVarValue::Plain(v.to_string()),
    };
    match broker {
        BrokerKind::Kafka | BrokerKind::Nats => vec![],
        BrokerKind::RabbitMq => vec![
            plain("RABBITMQ_DEFAULT_USER", &f.user),
            plain("RABBITMQ_DEFAULT_PASS", &f.password),
        ],
    }
}

fn broker_compose(broker: BrokerKind, svc: &str, img: &str, _f: &DbFormInput) -> String {
    match broker {
        BrokerKind::Kafka => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      KAFKA_NODE_ID: 1\n      KAFKA_PROCESS_ROLES: broker,controller\n      KAFKA_LISTENERS: PLAINTEXT://0.0.0.0:9092,CONTROLLER://0.0.0.0:9093\n      KAFKA_ADVERTISED_LISTENERS: PLAINTEXT://{svc}:9092\n      KAFKA_CONTROLLER_LISTENER_NAMES: CONTROLLER\n      KAFKA_LISTENER_SECURITY_PROTOCOL_MAP: CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT\n      KAFKA_CONTROLLER_QUORUM_VOTERS: 1@{svc}:9093\n      KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR: 1\n      KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR: 1\n      KAFKA_TRANSACTION_STATE_LOG_MIN_ISR: 1\n      KAFKA_GROUP_INITIAL_REBALANCE_DELAY_MS: 0\n    volumes:\n      - kafkadata:/var/lib/kafka/data\n\nvolumes:\n  kafkadata:\n"
        ),
        BrokerKind::RabbitMq => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      RABBITMQ_DEFAULT_USER: ${{RABBITMQ_DEFAULT_USER}}\n      RABBITMQ_DEFAULT_PASS: ${{RABBITMQ_DEFAULT_PASS}}\n    volumes:\n      - rabbitmqdata:/var/lib/rabbitmq\n\nvolumes:\n  rabbitmqdata:\n"
        ),
        BrokerKind::Nats => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    command: [\"-js\", \"-sd\", \"/data\"]\n    volumes:\n      - natsdata:/data\n\nvolumes:\n  natsdata:\n"
        ),
    }
}

// ── Storage (S3 compatível) ──────────────────────────────────────────────────

/// Servidores de objetos S3: RustFS e MinIO. Nascem como bancos/brokers: stack Compose
/// (`rp_<nome>`), credenciais geradas e guardadas como secret de serviço — o agente nunca as vê.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    RustFs,
    MinIo,
}

impl StorageKind {
    pub const ALL: &'static [StorageKind] = &[Self::RustFs, Self::MinIo];

    pub fn label(self) -> &'static str {
        match self {
            Self::RustFs => "RustFS",
            Self::MinIo => "MinIO",
        }
    }

    pub fn default_image(self) -> &'static str {
        match self {
            Self::RustFs => "rustfs/rustfs:latest",
            // `minio/minio` (Docker Hub e Quay) deixou de ser puxável ("access denied"; verificado
            // em 2026-11-03 com `podman manifest inspect`). O build do Chainguard roda com o mesmo
            // comando (`server /data --console-address ":9001"`) e as mesmas variáveis; foi testado
            // criando um bucket com credencial certa e recusando a errada.
            Self::MinIo => "cgr.dev/chainguard/minio:latest",
        }
    }

    /// Porta da API S3 (a que as apps usam).
    pub fn default_port(self) -> u16 {
        9000
    }

    /// Porta do console web.
    pub fn console_port(self) -> u16 {
        9001
    }

    pub fn kind_id(self) -> &'static str {
        match self {
            Self::RustFs => "rustfs",
            Self::MinIo => "minio",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "rustfs" => Some(Self::RustFs),
            "minio" => Some(Self::MinIo),
            _ => None,
        }
    }

    /// O usuário (access key) padrão, quando a pessoa não digita um.
    pub fn default_user(self) -> &'static str {
        match self {
            Self::RustFs => "rustfsadmin",
            Self::MinIo => "minioadmin",
        }
    }

    /// Nomes das duas envs de credencial: `(access key, secret key)`.
    pub fn credential_keys(self) -> (&'static str, &'static str) {
        match self {
            Self::RustFs => ("RUSTFS_ACCESS_KEY", "RUSTFS_SECRET_KEY"),
            Self::MinIo => ("MINIO_ROOT_USER", "MINIO_ROOT_PASSWORD"),
        }
    }
}

/// Passo "escolha o storage".
pub fn storage_rows_json() -> String {
    let rows: Vec<serde_json::Value> = StorageKind::ALL
        .iter()
        .map(|k| {
            json!({
                "id": k.kind_id(),
                "label": k.label(),
                "image": k.default_image(),
                "user": k.default_user(),
                "console_port": k.console_port(),
            })
        })
        .collect();
    serde_json::Value::Array(rows).to_string()
}

pub fn storage_spec(
    kind: StorageKind,
    name: String,
    project_id: String,
    f: &DbFormInput,
) -> ServiceSpec {
    let image = if f.image.trim().is_empty() {
        kind.default_image().to_string()
    } else {
        f.image.trim().to_string()
    };
    let svc = format!("rp_{}", shared::normalize_name(&name));
    let (access_key, secret_key) = kind.credential_keys();
    let plain = |k: &str, v: &str| EnvVar {
        key: k.to_string(),
        value: EnvVarValue::Plain(v.to_string()),
    };
    base_spec(
        name,
        project_id,
        ServiceSource::Compose(ComposeSource {
            content: storage_compose(kind, &svc, &image),
            ingress_service: None,
        }),
        kind.default_port(),
        vec![plain(access_key, &f.user), plain(secret_key, &f.password)],
        Some(kind.kind_id().to_string()),
    )
}

fn storage_compose(kind: StorageKind, svc: &str, img: &str) -> String {
    match kind {
        // Do blueprint de RustFS, sem o CORS `*` (quem precisar de CORS adiciona no Compose).
        StorageKind::RustFs => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      RUSTFS_ACCESS_KEY: ${{RUSTFS_ACCESS_KEY}}\n      RUSTFS_SECRET_KEY: ${{RUSTFS_SECRET_KEY}}\n      RUSTFS_ADDRESS: 0.0.0.0:9000\n      RUSTFS_CONSOLE_ADDRESS: 0.0.0.0:9001\n      RUSTFS_CONSOLE_ENABLE: \"true\"\n    volumes:\n      - rustfsdata:/data\n    command: /data\n\nvolumes:\n  rustfsdata:\n"
        ),
        StorageKind::MinIo => format!(
            "services:\n  {svc}:\n    image: {img}\n    restart: unless-stopped\n    environment:\n      MINIO_ROOT_USER: ${{MINIO_ROOT_USER}}\n      MINIO_ROOT_PASSWORD: ${{MINIO_ROOT_PASSWORD}}\n    volumes:\n      - miniodata:/data\n    command: server /data --console-address \":9001\"\n\nvolumes:\n  miniodata:\n"
        ),
    }
}

pub fn template_spec(
    t: &'static Template,
    name: String,
    project_id: String,
    values: &[String],
) -> ServiceSpec {
    let name = if name.trim().is_empty() {
        template_slug(t)
    } else {
        name
    };
    let user: Vec<(String, String)> = templates::editable_vars(t)
        .iter()
        .zip(values.iter())
        .map(|(v, val)| (v.key.to_string(), val.clone()))
        .collect();
    let rendered = templates::render(t, &user);
    let env_vars = rendered
        .env
        .into_iter()
        .map(|(key, value)| EnvVar {
            key,
            value: EnvVarValue::Plain(value),
        })
        .collect();
    let mut spec = base_spec(
        name,
        project_id,
        ServiceSource::Compose(ComposeSource {
            content: rendered.compose,
            ingress_service: None,
        }),
        rendered.port,
        env_vars,
        None,
    );
    spec.domain = rendered.domain.filter(|d| !d.is_empty());
    spec
}

// ── Requisição de criação (o cliente coleta os campos, o daemon monta o spec) ─

fn db_form(req: &WizardCreateReq) -> DbFormInput {
    DbFormInput {
        db_name: req.db_name.clone(),
        user: req.user.clone(),
        password: req.password.clone(),
        root_password: req.root_password.clone(),
        image: req.image.clone(),
        use_replica_sets: req.use_replica,
    }
}

/// Nome efetivo: `app_name` tem precedência sobre `name` (ambos trimados).
fn effective_name(req: &WizardCreateReq) -> String {
    if req.app_name.trim().is_empty() {
        req.name.trim().to_string()
    } else {
        req.app_name.trim().to_string()
    }
}

/// Monta o `ServiceSpec` a partir da requisição do wizard. `Err` para tipo/id
/// desconhecido.
pub fn build_spec(req: &WizardCreateReq) -> Result<ServiceSpec, String> {
    let mut spec = build_spec_inner(req)?;
    if req.expose_external {
        // Sentinela "aloque para mim": o handler service_create troca o 0 por
        // uma porta livre da faixa configurada antes de persistir.
        spec.host_port = Some(0);
    }
    Ok(spec)
}

fn build_spec_inner(req: &WizardCreateReq) -> Result<ServiceSpec, String> {
    let name = effective_name(req);
    match req.kind.as_str() {
        "application" => Ok(app_spec(name, req.project_id.clone())),
        "compose" => Ok(compose_spec(name, req.project_id.clone())),
        "database" => {
            let db = DbKind::from_str(&req.id)
                .ok_or_else(|| format!("banco desconhecido: {}", req.id))?;
            let mut form = db_form(req);
            // Quem chama sem preencher (o agente, via MCP) recebe os mesmos padrões que a GUI
            // pré-preenche: usuário padrão do motor e o banco "app".
            if db.has_user() && form.user.trim().is_empty() {
                form.user = db.default_user().to_string();
            }
            if db.has_db_name() && form.db_name.trim().is_empty() {
                form.db_name = "app".to_string();
            }
            completar_senhas(&mut form, true, db.has_root_password())?;
            Ok(db_spec(db, name, req.project_id.clone(), &form))
        }
        "broker" => {
            let b = BrokerKind::from_str(&req.id)
                .ok_or_else(|| format!("broker desconhecido: {}", req.id))?;
            let mut form = db_form(req);
            if b.has_user() && form.user.trim().is_empty() {
                form.user = b.default_user().to_string();
            }
            // Kafka e NATS não têm usuário/senha no wizard.
            completar_senhas(&mut form, b == BrokerKind::RabbitMq, false)?;
            Ok(broker_spec(b, name, req.project_id.clone(), &form))
        }
        "storage" => {
            let k = StorageKind::from_str(&req.id)
                .ok_or_else(|| format!("storage desconhecido: {}", req.id))?;
            let mut form = db_form(req);
            if form.user.trim().is_empty() {
                form.user = k.default_user().to_string();
            }
            completar_senhas(&mut form, true, false)?;
            Ok(storage_spec(k, name, req.project_id.clone(), &form))
        }
        "template" => {
            let t = templates::find(&req.id)
                .ok_or_else(|| format!("template desconhecido: {}", req.id))?;
            // template_spec usa o slug quando o nome vem vazio.
            Ok(template_spec(
                t,
                name,
                req.project_id.clone(),
                &req.template_values,
            ))
        }
        other => Err(format!("tipo de serviço desconhecido: {other}")),
    }
}

#[cfg(test)]
mod senhas_tests {
    use super::*;

    #[test]
    fn senha_gerada_tem_o_tamanho_e_o_alfabeto_e_nao_se_repete() {
        let a = generate_password(24).unwrap();
        let b = generate_password(24).unwrap();
        assert_eq!(a.len(), 24);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "{a}");
        assert_ne!(a, b, "duas senhas seguidas não podem coincidir");
        // Tamanhos que não são múltiplo do buffer também fecham.
        assert_eq!(generate_password(0).unwrap(), "");
        assert_eq!(generate_password(200).unwrap().len(), 200);
    }

    #[test]
    fn campo_vazio_gera_e_digitado_fica() {
        let mut f = DbFormInput::default();
        completar_senhas(&mut f, true, true).unwrap();
        assert_eq!(f.password.len(), PASSWORD_LEN);
        assert_eq!(f.root_password.len(), PASSWORD_LEN);
        assert_ne!(f.password, f.root_password);

        let mut g = DbFormInput {
            password: "minha-senha".into(),
            ..Default::default()
        };
        completar_senhas(&mut g, true, false).unwrap();
        assert_eq!(g.password, "minha-senha");
        assert!(g.root_password.is_empty(), "sem root quando não precisa");
    }

    #[test]
    fn wizard_gera_senha_para_banco_e_broker_mas_nao_para_kafka() {
        let req = |kind: &str, id: &str| WizardCreateReq {
            kind: kind.into(),
            id: id.into(),
            project_id: "p".into(),
            name: "x".into(),
            app_name: String::new(),
            db_name: String::new(),
            user: "u".into(),
            password: String::new(),
            root_password: String::new(),
            image: String::new(),
            use_replica: false,
            template_values: vec![],
            expose_external: false,
        };
        let pg = build_spec(&req("database", "postgres")).unwrap();
        let senha = pg
            .env_vars
            .iter()
            .find(|e| e.key == "POSTGRES_PASSWORD")
            .unwrap();
        assert!(matches!(&senha.value, EnvVarValue::Plain(v) if v.len() == PASSWORD_LEN));

        let my = build_spec(&req("database", "mysql")).unwrap();
        assert!(my.env_vars.iter().any(|e| e.key == "MYSQL_ROOT_PASSWORD"
            && matches!(&e.value, EnvVarValue::Plain(v) if v.len() == PASSWORD_LEN)));

        let rb = build_spec(&req("broker", "rabbitmq")).unwrap();
        assert!(rb.env_vars.iter().any(|e| e.key == "RABBITMQ_DEFAULT_PASS"
            && matches!(&e.value, EnvVarValue::Plain(v) if v.len() == PASSWORD_LEN)));

        assert!(
            build_spec(&req("broker", "kafka"))
                .unwrap()
                .env_vars
                .is_empty()
        );
    }

    #[test]
    fn campos_vazios_recebem_os_padroes_da_gui() {
        let vazio = |kind: &str, id: &str| WizardCreateReq {
            kind: kind.into(),
            id: id.into(),
            project_id: "p".into(),
            name: "x".into(),
            app_name: String::new(),
            db_name: String::new(),
            user: String::new(),
            password: String::new(),
            root_password: String::new(),
            image: String::new(),
            use_replica: false,
            template_values: vec![],
            expose_external: false,
        };
        let plain = |spec: &ServiceSpec, k: &str| {
            spec.env_vars
                .iter()
                .find(|e| e.key == k)
                .and_then(|e| match &e.value {
                    EnvVarValue::Plain(v) => Some(v.clone()),
                    _ => None,
                })
        };
        let pg = build_spec(&vazio("database", "postgres")).unwrap();
        assert_eq!(
            plain(&pg, "POSTGRES_USER").as_deref(),
            Some(DbKind::Postgres.default_user())
        );
        assert!(!plain(&pg, "POSTGRES_USER").unwrap().is_empty());
        assert_eq!(plain(&pg, "POSTGRES_DB").as_deref(), Some("app"));
        let rb = build_spec(&vazio("broker", "rabbitmq")).unwrap();
        assert_eq!(
            plain(&rb, "RABBITMQ_DEFAULT_USER").as_deref(),
            Some("rabbit")
        );
        // o que a pessoa digitou continua valendo
        let mut custom = vazio("database", "postgres");
        custom.user = "meu".into();
        custom.db_name = "meubanco".into();
        let pg2 = build_spec(&custom).unwrap();
        assert_eq!(plain(&pg2, "POSTGRES_USER").as_deref(), Some("meu"));
        assert_eq!(plain(&pg2, "POSTGRES_DB").as_deref(), Some("meubanco"));
    }

    #[test]
    fn storage_nasce_com_credenciais_geradas_e_sem_cors_aberto() {
        let req = |id: &str, user: &str| WizardCreateReq {
            kind: "storage".into(),
            id: id.into(),
            project_id: "p".into(),
            name: "arquivos".into(),
            app_name: String::new(),
            db_name: String::new(),
            user: user.into(),
            password: String::new(),
            root_password: String::new(),
            image: String::new(),
            use_replica: false,
            template_values: vec![],
            expose_external: false,
        };
        let get = |spec: &ServiceSpec, k: &str| {
            spec.env_vars
                .iter()
                .find(|e| e.key == k)
                .map(|e| match &e.value {
                    EnvVarValue::Plain(v) => v.clone(),
                    EnvVarValue::Secret(v) => v.clone(),
                })
        };
        let rustfs = build_spec(&req("rustfs", "")).unwrap();
        assert_eq!(rustfs.product.as_deref(), Some("rustfs"));
        assert_eq!(rustfs.port, 9000);
        assert_eq!(
            get(&rustfs, "RUSTFS_ACCESS_KEY").as_deref(),
            Some("rustfsadmin")
        );
        assert_eq!(
            get(&rustfs, "RUSTFS_SECRET_KEY").unwrap().len(),
            PASSWORD_LEN
        );
        let ServiceSource::Compose(c) = &rustfs.source else {
            panic!("storage é Compose")
        };
        assert!(c.content.contains("rp_arquivos:") && c.content.contains("command: /data"));
        assert!(!c.content.contains("CORS"), "sem CORS aberto por padrão");

        let minio = build_spec(&req("minio", "meu-usuario")).unwrap();
        assert_eq!(
            get(&minio, "MINIO_ROOT_USER").as_deref(),
            Some("meu-usuario")
        );
        assert_eq!(
            get(&minio, "MINIO_ROOT_PASSWORD").unwrap().len(),
            PASSWORD_LEN
        );

        assert!(build_spec(&req("s3-inexistente", "")).is_err());
        let rows: serde_json::Value = serde_json::from_str(&storage_rows_json()).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
    }
}
