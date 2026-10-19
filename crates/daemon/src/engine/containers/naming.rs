//! Nomes dos containers de serviços Application: live, staging, réplicas e o formato legado.

pub(super) fn with_replica_suffix(base: String, idx: u32) -> String {
    if idx == 0 {
        base
    } else {
        format!("{base}_r{idx}")
    }
}

pub fn staging_name(service_id: &str, service_name: &str, deployment_id_short: &str) -> String {
    replica_staging_name(service_id, service_name, deployment_id_short, 0)
}

pub fn replica_staging_name(
    service_id: &str,
    service_name: &str,
    dep_short: &str,
    idx: u32,
) -> String {
    let base = shared::app_container_base(service_id, service_name);
    with_replica_suffix(format!("{base}_staging_{dep_short}"), idx)
}

pub fn replica_live_name(service_id: &str, service_name: &str, idx: u32) -> String {
    with_replica_suffix(shared::app_container_base(service_id, service_name), idx)
}

/// Nome de staging no formato legado (`rp_<safe>_staging_<dep>[_r<i>]`).
pub fn legacy_replica_staging_name(service_name: &str, dep_short: &str, idx: u32) -> String {
    let safe_name = shared::normalize_name(service_name);
    with_replica_suffix(format!("rp_{safe_name}_staging_{dep_short}"), idx)
}

/// É um staging (nome termina em `_staging_<dep>` ou `_staging_<dep>_r<i>`)?
/// Staging promovido é renomeado, e perde o marcador.
///
/// `<dep>` são os 8 primeiros caracteres do ULID do deploy — maiúsculas e
/// dígitos. O nome do serviço entra normalizado, sempre minúsculo; é isso que
/// impede o live de um serviço chamado `app-staging-prod` de ser tomado por
/// staging.
pub(super) fn is_staging_name(name: &str) -> bool {
    let Some((_, tail)) = name.rsplit_once("_staging_") else {
        return false;
    };
    let dep = tail.split_once("_r").map_or(tail, |(d, _)| d);
    dep.len() == 8
        && dep
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
}

/// O label `rustploy.service_id` de um container identifica `service_id`?
///
/// Aceita também o formato de antes dos prefixos de tipo nos IDs (commit
/// 8a6d27f): containers criados naquela época carregam o ULID sem o `svc_`.
pub fn label_owned_by(label: &str, service_id: &str) -> bool {
    label == service_id || service_id.strip_prefix("svc_") == Some(label)
}
