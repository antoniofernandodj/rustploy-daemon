//! Resolução de diretórios (banco, chave age) com fallback para o home e inicialização do log.

use std::path::PathBuf;
use tracing::warn;
use tracing_subscriber::EnvFilter;

pub fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .compact()
        .init();
}

fn fallback_dir() -> PathBuf {
    crate::platform::config::fallback_data_dir()
}

/// Tries to use `configured` as the data directory.
/// Falls back to `~/.local/share/rustploy/db` if the path is not writable.
pub fn resolve_data_path(configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if can_write_dir(&path) {
        return path;
    }
    let fallback = fallback_dir().join("db");
    warn!(
        primary = %path.display(),
        fallback = %fallback.display(),
        "db path not writable, using fallback"
    );
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Onde fica o arquivo da identidade `age` do servidor: `age_key_path`; senão `/etc/rustploy/age.key`. Se o arquivo não
/// existe e a pasta não é gravável (ex.: `/etc/rustploy/` sem root), cai para
/// `~/.local/share/rustploy/age.key`.
pub fn resolve_age_key_path(cfg: &crate::platform::config::SecretsConfig) -> PathBuf {
    let path = PathBuf::from(
        cfg.age_key_path
            .as_deref()
            .unwrap_or("/etc/rustploy/age.key"),
    );
    if path.exists() {
        return path;
    }
    let parent = path.parent().unwrap_or(&path);
    if can_write_dir(parent) {
        return path;
    }
    let fallback = fallback_dir().join("age.key");
    warn!(
        primary = %path.display(),
        fallback = %fallback.display(),
        "age key directory not writable, using fallback"
    );
    let _ = std::fs::create_dir_all(fallback.parent().unwrap());
    fallback
}

/// Returns true only when `dir` (or its path) is both creatable and writable.
/// Unlike a bare `create_dir_all` check, this actually probes write access
/// even when the directory already exists (e.g. created by a previous root run).
fn can_write_dir(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".rustploy_write_probe");
    if std::fs::write(&probe, b"").is_err() {
        return false;
    }
    let _ = std::fs::remove_file(probe);
    true
}

#[cfg(test)]
mod age_key_tests {
    use super::*;
    use crate::platform::config::SecretsConfig;

    fn parse(toml_text: &str) -> SecretsConfig {
        #[derive(serde::Deserialize)]
        struct W {
            secrets: SecretsConfig,
        }
        toml::from_str::<W>(toml_text).unwrap().secrets
    }

    #[test]
    fn config_sem_age_key_path_parseia_e_usa_o_padrao() {
        let c = parse("[secrets]\nrecovery_recipient = \"age1x\"\n");
        assert!(c.age_key_path.is_none());
        assert_eq!(resolve_age_key_path(&c).file_name().unwrap(), "age.key");
    }

    #[test]
    fn age_key_path_explicito_tem_prioridade() {
        let d = tempfile::tempdir().unwrap();
        let c = parse(&format!(
            "[secrets]\nage_key_path = \"{}/k.key\"\n",
            d.path().display()
        ));
        assert_eq!(resolve_age_key_path(&c), d.path().join("k.key"));
    }
}
