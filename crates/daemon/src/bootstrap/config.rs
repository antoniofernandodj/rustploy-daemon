//! Resolução de diretórios (banco, chave mestra) com fallback para o home e inicialização do log.

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

/// Tries to use `configured` as the master key path.
/// Falls back to `~/.local/share/rustploy/master.key` if the directory is
/// not writable (e.g. `/etc/rustploy/` requires root).
pub fn resolve_master_key_path(configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    // If the key already exists and is readable, use it as-is.
    if path.exists() {
        return path;
    }
    // Otherwise we need to be able to create it — check parent writability.
    let parent = path.parent().unwrap_or(&path);
    if can_write_dir(parent) {
        return path;
    }
    let fallback = fallback_dir().join("master.key");
    warn!(
        primary = %path.display(),
        fallback = %fallback.display(),
        "master key directory not writable, using fallback"
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
