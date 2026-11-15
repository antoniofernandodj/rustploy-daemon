//! `Command::InfraAudit`: o que está em claro no estado de infra (sem os valores).

use crate::bootstrap::state::AppState;
use crate::domain::PlaintextFinding;
use crate::features::state_git::StateGit;
use crate::protocol::Response;
use crate::store::infra::audit::{Certainty, plaintext_findings};

pub async fn handle(state: AppState) -> Response {
    let allowed = state.state_git.allowed().await;
    let findings = plaintext_findings(&state.db.infra().snapshot())
        .into_iter()
        .map(|f| PlaintextFinding {
            definite: f.certainty == Certainty::Definite,
            allowed: f.certainty == Certainty::Suspect
                && allowed.contains(&StateGit::fingerprint(&f.project, &f.scope, &f.what)),
            project: f.project,
            scope: f.scope,
            what: f.what,
        })
        .collect();
    Response::InfraAudit(findings)
}
