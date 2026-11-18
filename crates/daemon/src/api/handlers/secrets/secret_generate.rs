//! `Command::SecretGenerate`: sorteia e grava um secret cifrado sem que o valor saia do daemon.

use crate::bootstrap::state::AppState;
use crate::platform::secrets::{GenerateOutcome, Scope};
use crate::protocol::{Response, SecretKind, SecretOnExists};

#[allow(clippy::too_many_arguments)]
pub async fn handle(
    state: AppState,
    project_id: String,
    service_id: Option<String>,
    job_id: Option<String>,
    name: String,
    kind: SecretKind,
    length: Option<u32>,
    on_exists: SecretOnExists,
) -> Response {
    let scope = match Scope::from_ids(service_id.as_deref(), job_id.as_deref()) {
        Ok(scope) => scope,
        Err(e) => return Response::err("InvalidScope", e.to_string()),
    };
    match state
        .secrets
        .generate_scoped(&project_id, scope, &name, kind, length, on_exists)
        .await
    {
        Ok(GenerateOutcome::Created(generated)) => Response::SecretGenerated {
            name,
            created: true,
            length: generated.length,
            fingerprint: generated.fingerprint,
        },
        Ok(GenerateOutcome::KeptExisting(generated)) => Response::SecretGenerated {
            name,
            created: false,
            length: generated.length,
            fingerprint: generated.fingerprint,
        },
        Ok(GenerateOutcome::Exists) => Response::err(
            "SecretExists",
            format!(
                "o secret '{name}' já existe; use on_exists \"Keep\" para mantê-lo ou \"Replace\" para trocar o valor"
            ),
        ),
        Err(e) => Response::err("SecretGenerateFailed", e.to_string()),
    }
}
