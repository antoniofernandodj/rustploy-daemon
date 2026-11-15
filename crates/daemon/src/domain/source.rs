use serde::{Deserialize, Serialize};

/// Heuristic used by the clients' General tab to decide whether a
/// "Repository URL / Image" value denotes a Git source to clone+build, or a
/// Docker image to pull. Recognizes the common git URL schemes, including
/// local `file://` repositories (which the daemon clones via `git clone`).
pub fn looks_like_git_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("https://")
        || u.starts_with("http://")
        || u.starts_with("git@")
        || u.starts_with("ssh://")
        || u.starts_with("file://")
        || u.ends_with(".git")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServiceSource {
    Registry { image: String },
    Git(GitSource),
    Archive(ArchiveSource),
    Compose(ComposeSource),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchiveSource {
    /// Identifier of the last uploaded zip stored under the daemon data dir.
    pub archive_id: String,
    #[serde(default)]
    pub original_filename: Option<String>,
    /// Dockerfile path inside the extracted archive. The GUI defaults this to
    /// `Dockerfile`; deploy validation requires it to exist.
    pub dockerfile_path: String,
    /// Docker build context inside the extracted archive.
    pub build_context: String,
}

impl Default for ArchiveSource {
    fn default() -> Self {
        Self {
            archive_id: String::new(),
            original_filename: None,
            dockerfile_path: "Dockerfile".into(),
            build_context: ".".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ComposeSource {
    #[serde(alias = "compose_file")]
    pub content: String,
    /// Nome do serviço **dentro do arquivo compose** que recebe o tráfego do
    /// ingress (ex.: `kong` numa stack Supabase, `nginx` num LEMP).
    ///
    /// Uma stack Compose tem N containers e só um deles atende o domínio. Sem
    /// isto o daemon descobre o alvo pela porta (ver
    /// `docker::containers::find_compose_ingress_container`); preencha quando a
    /// descoberta errar ou quando dois containers expuserem a mesma porta.
    #[serde(default)]
    pub ingress_service: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitSource {
    pub url: String,
    pub branch: String,
    pub root_path: String,
    pub watch_paths: Vec<String>,
    pub submodules: bool,
    pub dockerfile_path: String,
    pub build_context: String,
    pub build_stage: Option<String>,
    pub credentials: Option<String>,
    pub username: Option<String>,
    /// When set, deploys resolve the clone token from the connected Git
    /// provider (Gitea OAuth/PAT) instead of (or in addition to) `credentials`.
    #[serde(default)]
    pub provider_id: Option<String>,
}

impl Default for GitSource {
    fn default() -> Self {
        Self {
            url: String::new(),
            branch: "main".into(),
            root_path: ".".into(),
            watch_paths: vec![],
            submodules: false,
            dockerfile_path: "Dockerfile".into(),
            build_context: ".".into(),
            build_stage: None,
            credentials: None,
            username: None,
            provider_id: None,
        }
    }
}

// ── Git providers (Gitea OAuth2 / PAT) ────────────────────────────────────────
