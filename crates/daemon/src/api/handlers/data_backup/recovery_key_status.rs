//! `Command::RecoveryKeyStatus`: há chave de recuperação e qual é a pública (nunca a privada).

use crate::bootstrap::state::AppState;
use shared::Response;

pub fn handle(state: &AppState) -> Response {
    let public_key = state.secrets.recovery_public_key();
    Response::RecoveryKey {
        configured: public_key.is_some(),
        public_key,
    }
}
