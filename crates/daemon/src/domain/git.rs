use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which hosted Git service a provider connects to. Gitea and GitHub are
/// implemented; the enum exists so GitLab (etc.) can be added without a wire
/// break. New variants MUST be appended at the end — the JSON tag is the
/// variant name (`"Gitea"`/`"Github"`), so order only matters for postcard.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitProviderKind {
    Gitea,
    Github,
}

impl GitProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gitea => "gitea",
            Self::Github => "github",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "gitea" => Some(Self::Gitea),
            "github" => Some(Self::Github),
            _ => None,
        }
    }
}

/// How a provider authenticates: full OAuth2 authorization-code flow, or a
/// pasted Personal Access Token.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitAuthMode {
    OAuth,
    Pat,
}

impl GitAuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::Pat => "pat",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "oauth" => Some(Self::OAuth),
            "pat" => Some(Self::Pat),
            _ => None,
        }
    }
}

/// The connected account, populated once OAuth completes (or the PAT validates).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitAccount {
    pub login: String,
    pub avatar_url: Option<String>,
}

/// A connected Git provider, as exposed to clients. Secrets (client secret,
/// access/refresh tokens, PAT) are kept daemon-side and never serialized here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitProvider {
    pub id: String,
    pub kind: GitProviderKind,
    pub name: String,
    pub base_url: String,
    pub auth_mode: GitAuthMode,
    pub oauth_client_id: Option<String>,
    /// `Some` once the connection is established.
    pub account: Option<GitAccount>,
    pub created_at: DateTime<Utc>,
}

/// A repository listed from a provider's API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitRepo {
    pub full_name: String,
    pub clone_url: String,
    pub default_branch: String,
    pub private: bool,
}

/// A branch listed from a provider's API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitBranch {
    pub name: String,
}

#[cfg(test)]
mod git_provider_tests {
    use super::*;
    use crate::domain::*;
    use crate::protocol::{Command, Response};

    #[test]
    fn git_source_json_back_compat_without_provider_id() {
        // Specs persisted before the feature lack `provider_id`; serde default
        // must fill it as None.
        let json = r#"{
            "url":"https://gitea.test/u/r.git","branch":"main","root_path":".",
            "watch_paths":[],"submodules":false,"dockerfile_path":"Dockerfile",
            "build_context":".","build_stage":null,"credentials":null,"username":null
        }"#;
        let g: GitSource = serde_json::from_str(json).unwrap();
        assert_eq!(g.provider_id, None);
        assert_eq!(g.branch, "main");
    }

    #[test]
    fn command_json_round_trip() {
        let cmd = Command::GitProviderCreate {
            kind: GitProviderKind::Gitea,
            name: "Gitea".into(),
            base_url: "https://gitea.test".into(),
            auth_mode: GitAuthMode::OAuth,
            oauth_client_id: Some("cid".into()),
            oauth_client_secret: Some("secret".into()),
            pat: None,
        };
        let s = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Command::GitProviderCreate {
                kind: GitProviderKind::Gitea,
                ..
            }
        ));
    }

    #[test]
    fn response_json_round_trip() {
        let resp = Response::GitProviders(vec![GitProvider {
            id: "1".into(),
            kind: GitProviderKind::Gitea,
            name: "Gitea".into(),
            base_url: "https://gitea.test".into(),
            auth_mode: GitAuthMode::Pat,
            oauth_client_id: None,
            account: Some(GitAccount {
                login: "alice".into(),
                avatar_url: None,
            }),
            created_at: Utc::now(),
        }]);
        let s = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back {
            Response::GitProviders(v) => assert_eq!(v[0].account.as_ref().unwrap().login, "alice"),
            _ => panic!("variante errada"),
        }
    }

    #[test]
    fn service_status_queued_serde_and_display() {
        // A GUI (Luau) casa o status pela string JSON — Queued é variante unit,
        // então serializa como "Queued". O Display casa a gravação no banco.
        assert_eq!(
            serde_json::to_string(&ServiceStatus::Queued).unwrap(),
            "\"Queued\""
        );
        let back: ServiceStatus = serde_json::from_str("\"Queued\"").unwrap();
        assert_eq!(back, ServiceStatus::Queued);
        assert_eq!(ServiceStatus::Queued.to_string(), "Queued");
    }
}
