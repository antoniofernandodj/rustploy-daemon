//! `Command::RecoveryKeyGenerate`: gera a chave de recuperação; a privada sai só nesta resposta.
//! Plano: `docs/plano-chave-de-recuperacao.md`.

use crate::bootstrap::state::AppState;
use shared::Response;

pub fn handle(state: &AppState, replace: bool) -> Response {
    match state.secrets.generate_recovery(replace) {
        Ok((public_key, private_key)) => {
            // Os segredos que já existem estão cifrados só para a chave do servidor (ou para a chave de
            // recuperação ANTERIOR): recifra-os para incluir a nova, senão ela só abriria o que for
            // gravado daqui em diante. A chave já está gravada e a privada já existe, então uma falha aqui
            // não desfaz nada: vai para o log (e o ciclo normal recifra o que for regravado).
            match crate::store::infra::rekey::rekey_all(state.db.infra(), &state.secrets) {
                Ok(n) => tracing::info!(valores = n, "chave de recuperação: segredos recifrados"),
                Err(e) => {
                    tracing::warn!("chave de recuperação: não consegui recifrar os segredos: {e:#}")
                }
            }
            let host = server_name();
            // Mesmo formato do `rustploy-cli keygen`: o `secret decrypt -i` aceita o arquivo direto.
            let file_contents = format!(
                "# Rustploy — chave de recuperação do servidor {host}\n\
                 # Guarde em um cofre/gerenciador de senhas. É a ÚNICA cópia: o servidor não a guarda.\n\
                 # chave pública: {public_key}\n\
                 {private_key}\n"
            );
            Response::RecoveryKeyGenerated {
                suggested_filename: format!(
                    "rustploy-recovery-key-{host}-{}.txt",
                    chrono::Utc::now().format("%Y%m%d")
                ),
                public_key,
                private_key,
                file_contents,
            }
        }
        Err(e) => Response::err("RecoveryKeyError", format!("{e:#}")),
    }
}

/// Nome do servidor para o arquivo: o hostname, só com letras/dígitos/`-`/`.`.
fn server_name() -> String {
    let raw = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let clean: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
        .collect();
    if clean.is_empty() {
        "servidor".to_string()
    } else {
        clean
    }
}
