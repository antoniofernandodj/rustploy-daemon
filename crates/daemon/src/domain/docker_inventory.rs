use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::*;

/// One Docker image (from `docker system df`), independent of whether it's
/// currently used by any service the daemon manages — the Docker tab lists
/// every image on the host, not just rustploy's own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerImageInfo {
    pub id: String,
    /// Repo:tag references; empty when the image is untagged (`<none>`, a
    /// dangling layer left behind by a superseded build).
    pub tags: Vec<String>,
    pub size_bytes: u64,
    pub created: DateTime<Utc>,
    /// Containers (running or stopped) referencing this image. `docker
    /// system df` always computes this (unlike the plain image-list
    /// endpoint, which leaves it `-1` unless asked).
    pub containers: i64,
    /// Best-effort project/service this image belongs to, inferred from its
    /// tag (`rp_<safe_name>:...` for Git builds, exact string match for
    /// registry images). `None` when nothing in the daemon's DB matches —
    /// e.g. a manually-pulled image, or one left over from a deleted service.
    pub project: Option<String>,
    pub service: Option<String>,
}

/// One Docker volume, independent of rustploy ownership (rustploy itself
/// only ever bind-mounts host paths — see `docker/containers.rs` — so named
/// volumes here are either created by the user directly or implicitly by an
/// image's own `VOLUME` directive).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerVolumeInfo {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    /// `false` when no container currently references the volume — the
    /// "unused" set `docker volume prune` would remove.
    pub in_use: bool,
    /// Containers referencing this volume, when Docker reports it (`-1`
    /// when not computed/available for this driver).
    pub ref_count: i64,
    /// Disk usage in bytes, when available (`-1` otherwise — e.g. non-`local`
    /// drivers never report a size).
    pub size_bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerContainerInfo {
    /// Full container id (for removal).
    pub id: String,
    /// Primary name, sem a barra inicial que o Docker prefixa.
    pub name: String,
    pub image: String,
    /// Estado bruto do Docker: "running", "exited", "created", "paused", …
    pub state: String,
    /// Linha de status legível do Docker (ex.: "Exited (0) 2 hours ago").
    pub status: String,
    /// `true` quando tem o label `rustploy.managed=true`.
    pub managed: bool,
    pub project: Option<String>,
    pub service: Option<String>,
    /// `Some(nome)` quando o serviço dono deste container **sumiu do estado por um commit externo**
    /// (a sincronização com o git não para container nem apaga dado por conta própria). É o sinal de
    /// "órfão": pare e remova daqui. Se o serviço voltar (rollback no git), a marca some e o
    /// container volta a ser dele.
    #[serde(default)]
    pub removed_in_git: Option<String>,
}

/// One Docker network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerNetworkInfo {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    /// `false` when no container is attached — the "unused" set `docker
    /// network prune` would remove (rustploy's own per-project networks
    /// included, once their last service is gone).
    pub in_use: bool,
    pub container_count: usize,
    /// Project this network belongs to, matched by the
    /// name stored in `project.network_name` (see
    /// `db::projects::network_names`). `None` for non-rustploy
    /// networks (the built-in `bridge`/`host`/`none`, or manually created ones).
    pub project: Option<String>,
}

// ── Registry OCI embutido (Docker > Registry) ─────────────────────────────────

pub const DEFAULT_CLEANUP_RETENTION_HOURS: u32 = 168;

fn default_warm_after_cleanup() -> bool {
    true
}

fn default_cleanup_retention_hours() -> u32 {
    DEFAULT_CLEANUP_RETENTION_HOURS
}

/// Configuração de limpeza automática de recursos Docker não usados
/// (containers parados, imagens, volumes, redes, cache de build) — ver
/// `docs/plano-limpeza-automatica-docker.md`. É uma configuração singleton
/// do daemon (não uma lista de itens), guardada como um único JSON em
/// `daemon_settings` (`KEY_DOCKER_CLEANUP_CONFIG`) — sem tabela própria.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DockerCleanupConfig {
    pub enabled: bool,
    pub recurrence: Recurrence,
    pub containers: bool,
    pub images: bool,
    /// Sem isto, só remove imagens "dangling" (sem tag) — mesma semântica do
    /// parâmetro `all` de `docker_prune::prune_images`.
    pub images_all: bool,
    pub volumes: bool,
    /// Volumes de fora dos serviços gerenciados pelo rustploy (que só usam
    /// bind mount) podem conter dado — ver aviso no doc do plano.
    pub volumes_all: bool,
    pub networks: bool,
    pub build_cache: bool,
    /// Só remove cache de build sem uso há mais de N horas; `0` remove tudo
    /// (comportamento anterior). `#[serde(default)]`: a config já salva não
    /// tem a chave, e sem default o parse falharia e a config voltaria ao
    /// padrão (desligada).
    #[serde(default = "default_cleanup_retention_hours")]
    pub build_cache_older_than_hours: u32,
    /// Só remove imagens criadas há mais de N horas; `0` sem filtro. O filtro
    /// do Docker usa a data de CRIAÇÃO da imagem, não a do último uso.
    #[serde(default = "default_cleanup_retention_hours")]
    pub images_older_than_hours: u32,
    /// Depois da limpeza, reconstrói a imagem de cada serviço com fonte Git ou
    /// zip, na fila de deploy, pra o primeiro deploy do dia achar o cache de
    /// build quente. `#[serde(default = ...)]`: a config já salva não tem a
    /// chave e passa a valer `true` (o problema que isto resolve é de quem já
    /// usa a limpeza).
    #[serde(default = "default_warm_after_cleanup")]
    pub warm_after_cleanup: bool,
    /// `#[serde(default)]`: os clientes (Luau/JS) mandam a chave inteira ao
    /// fazer round-trip do valor recebido, mas o formulário de criação (antes
    /// do primeiro `Get`) pode omiti-la de vez — mesma convenção de
    /// `JobGitSource`.
    #[serde(default)]
    pub last_run_at: Option<DateTime<Utc>>,
    /// Calculado e persistido junto (nunca recomputado só a partir de "agora"
    /// a cada tick do scheduler — ver `recompute_next_run`): igual ao
    /// `Job.next_run_at`, é o que faz "nunca rodou ainda" convergir pro
    /// próximo horário da recorrência em vez de rodar assim que é ativado.
    /// Sempre recalculado no servidor em `DockerCleanupConfigSet` — o que o
    /// cliente mandar aqui é ignorado.
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
}

impl Default for DockerCleanupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            recurrence: Recurrence::Daily { hour: 3, minute: 0 },
            containers: false,
            images: false,
            images_all: false,
            volumes: false,
            volumes_all: false,
            networks: false,
            build_cache: false,
            build_cache_older_than_hours: DEFAULT_CLEANUP_RETENTION_HOURS,
            images_older_than_hours: DEFAULT_CLEANUP_RETENTION_HOURS,
            warm_after_cleanup: true,
            last_run_at: None,
            next_run_at: None,
        }
    }
}

impl DockerCleanupConfig {
    pub fn any_resource_enabled(&self) -> bool {
        self.containers || self.images || self.volumes || self.networks || self.build_cache
    }

    /// Recalcula `next_run_at` a partir de `last_run_at` (ou de `now` quando
    /// nunca rodou) — chamado ao salvar a config e depois de cada execução.
    /// `None` quando desativado ou sem nenhum recurso marcado, pro scheduler
    /// não ter que checar essas duas condições de novo em todo tick.
    pub fn recompute_next_run(&mut self, now: DateTime<Utc>) {
        self.next_run_at = if self.enabled && self.any_resource_enabled() {
            Some(self.recurrence.next_after(self.last_run_at.unwrap_or(now)))
        } else {
            None
        };
    }
}

/// Resultado da limpeza de UM recurso, dentro de uma execução — usado tanto
/// no resumo persistido (`DockerCleanupLastRun`) quanto no
/// `Event::DockerCleanupCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DockerCleanupResourceResult {
    /// "containers" | "images" | "volumes" | "networks" | "build_cache".
    pub resource: String,
    pub count: u32,
    pub reclaimed_bytes: u64,
    /// `Some` quando esse recurso falhou — os demais continuam rodando (uma
    /// falha em "volumes" não impede a limpeza de "images", por exemplo).
    pub error: Option<String>,
}

/// Resumo persistido da última execução (chave `KEY_DOCKER_CLEANUP_LAST_RUN`
/// em `daemon_settings`), pra tela mostrar "última limpeza: ..." sem
/// depender de ainda estar conectada ao SSE quando ela rodou.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DockerCleanupLastRun {
    pub at: DateTime<Utc>,
    pub results: Vec<DockerCleanupResourceResult>,
}
