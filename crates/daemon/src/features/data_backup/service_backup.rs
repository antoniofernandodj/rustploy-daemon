//! Backup dos dados de **um serviço** para o S3 e a restauração a partir dele
//! (`docs/plano-backup-de-dados-para-s3.md`, Fase 2).
//!
//! O conteúdo é o pacote `.rpdata` de sempre (`service_data::transfer::export`), **sempre cifrado** com `age`
//! para a identidade do servidor e a chave de recuperação: sem a chave de recuperação o backup não abriria
//! num servidor novo, então ele nem começa. Operações longas rodam em segundo plano; o resultado de cada
//! uma fica gravado por serviço (`service_backup_run:<id>`) e a tela o lê em `ServiceBackupList`.
//!
//! Chave do objeto: `<prefixo>/<project_id>/<service_id>/<AAAAMMDD-HHMMSS>.rpdata.age`. Os IDs vão para o
//! YAML e o `state_git` os preserva, então um servidor restaurado enxerga os backups do original.

use super::destination;
use crate::features::deploy::DeployContext;
use crate::features::service_data::{parts, transfer};
use crate::platform::s3::{self, S3Config};
use crate::store::daemon_settings;
use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use shared::{BackupEntry, BackupRun, ServiceBackups};
use tracing::{info, warn};

/// Uma operação "em andamento" há mais que isto é lixo de um daemon que caiu no meio: a tela a mostra
/// como interrompida em vez de travada para sempre.
const STALE_RUNNING_HOURS: i64 = 12;

/// A "pasta" do serviço no bucket, sempre terminada em `/`.
pub fn service_prefix(dest_prefix: &str, project_id: &str, service_id: &str) -> String {
    let base = dest_prefix.trim_matches('/');
    if base.is_empty() {
        format!("{project_id}/{service_id}/")
    } else {
        format!("{base}/{project_id}/{service_id}/")
    }
}

fn new_key(prefix: &str) -> String {
    format!("{prefix}{}.rpdata.age", Utc::now().format("%Y%m%d-%H%M%S"))
}

/// A chave pertence mesmo a este serviço? Barra `..` e chaves de outro serviço: restaurar/apagar só
/// mexe no que está na pasta do serviço.
pub fn key_belongs(prefix: &str, key: &str) -> bool {
    key.starts_with(prefix) && !key.contains("..") && key.len() > prefix.len()
}

/// Quais chaves saem para sobrar só as `keep` mais novas. O nome tem a data (`AAAAMMDD-HHMMSS`), então
/// a ordem lexical é a cronológica. Nunca devolve `keep` < 1 itens a manter.
pub fn to_prune(keys: &[String], keep: u32) -> Vec<String> {
    let mut sorted: Vec<&String> = keys.iter().collect();
    sorted.sort();
    sorted.reverse();
    sorted
        .into_iter()
        .skip(keep.max(1) as usize)
        .cloned()
        .collect()
}

// ── o registro da última operação ───────────────────────────────────────────

fn run_key(service_id: &str) -> String {
    format!("service_backup_run:{service_id}")
}

pub async fn last_run(ctx: &DeployContext, service_id: &str) -> Option<BackupRun> {
    let raw = daemon_settings::get(&ctx.db, &run_key(service_id))
        .await
        .ok()??;
    let mut run: BackupRun = serde_json::from_str(&raw).ok()?;
    if run.state == "running"
        && chrono::DateTime::parse_from_rfc3339(&run.started_at)
            .map(|t| Utc::now().signed_duration_since(t).num_hours() >= STALE_RUNNING_HOURS)
            .unwrap_or(false)
    {
        run.state = "failed".into();
        run.error = Some("interrompido (o daemon reiniciou no meio)".into());
    }
    Some(run)
}

async fn save_run(ctx: &DeployContext, service_id: &str, run: &BackupRun) {
    if let Ok(json) = serde_json::to_string(run)
        && let Err(e) = daemon_settings::set(&ctx.db, &run_key(service_id), &json).await
    {
        warn!(service_id, error = %e, "não consegui gravar o resultado do backup");
    }
}

fn last_start_key(service_id: &str) -> String {
    format!("service_backup_last_start:{service_id}")
}

/// Quando o último backup deste serviço **começou** (manual ou agendado): é daqui que o agendador conta o
/// próximo horário.
pub async fn last_start(ctx: &DeployContext, service_id: &str) -> Option<chrono::DateTime<Utc>> {
    let raw = daemon_settings::get(&ctx.db, &last_start_key(service_id))
        .await
        .ok()??;
    chrono::DateTime::parse_from_rfc3339(&raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

async fn mark_start(ctx: &DeployContext, service_id: &str) {
    let _ = daemon_settings::set(
        &ctx.db,
        &last_start_key(service_id),
        &Utc::now().to_rfc3339(),
    )
    .await;
}

/// Um disparo do agendador que nem chegou a começar (sem destino, serviço ocupado…): conta como
/// tentativa, para o agendador esperar o horário seguinte, e mostra o motivo na tela. Não pisa numa
/// operação que está de fato em andamento.
pub async fn note_attempt(ctx: &DeployContext, service_id: &str, error: &str) {
    mark_start(ctx, service_id).await;
    if last_run(ctx, service_id)
        .await
        .is_some_and(|r| r.state == "running")
    {
        return;
    }
    let run = finish(running("backup"), Err(anyhow!("{error}")));
    save_run(ctx, service_id, &run).await;
}

fn running(op: &str) -> BackupRun {
    BackupRun {
        op: op.into(),
        state: "running".into(),
        started_at: Utc::now().to_rfc3339(),
        finished_at: None,
        size_bytes: None,
        key: None,
        error: None,
        checks: vec![],
    }
}

fn finish(mut run: BackupRun, result: Result<()>) -> BackupRun {
    run.finished_at = Some(Utc::now().to_rfc3339());
    match result {
        Ok(()) => run.state = "ok".into(),
        Err(e) => {
            run.state = "failed".into();
            run.error = Some(format!("{e:#}"));
        }
    }
    run
}

// ── pré-condições ───────────────────────────────────────────────────────────

struct Ready {
    cfg: S3Config,
    prefix: String,
}

/// O que tem de estar de pé antes de qualquer operação: serviço, destino S3 e chave de recuperação.
async fn ready(ctx: &DeployContext, service_id: &str) -> Result<Ready> {
    let svc = crate::store::services::get(&ctx.db, service_id)
        .await?
        .ok_or_else(|| anyhow!("serviço não encontrado"))?;
    if !ctx.secrets.has_recovery() {
        bail!(
            "configure a chave de recuperação ([secrets] recovery_recipient no config.toml): o backup \
             é sempre cifrado, e sem ela só este servidor o abriria"
        );
    }
    let (cfg, dest_prefix) = destination::resolve(&ctx.db, &ctx.secrets, None).await?;
    Ok(Ready {
        prefix: service_prefix(&dest_prefix, &svc.spec.project_id, service_id),
        cfg,
    })
}

// ── listar ──────────────────────────────────────────────────────────────────

pub async fn list(ctx: DeployContext, service_id: &str) -> ServiceBackups {
    let last_run = last_run(&ctx, service_id).await;
    let infra = super::policy::get(&ctx.db, service_id);
    let next_run_at = match &infra {
        Some(b) if b.policy.enabled => {
            Some(super::policy::next_run(b, last_start(&ctx, service_id).await).to_rfc3339())
        }
        _ => None,
    };
    let policy = infra.map(|b| b.policy);
    let r = match ready(&ctx, service_id).await {
        Ok(r) => r,
        Err(e) => {
            return ServiceBackups {
                last_run,
                blocked: Some(e.to_string()),
                policy,
                next_run_at,
                ..Default::default()
            };
        }
    };
    let mut out = ServiceBackups {
        last_run,
        policy,
        next_run_at,
        ..Default::default()
    };
    // sem dados, não há o que salvar (os backups antigos continuam listados)
    if let Ok(Some(svc)) = crate::store::services::get(&ctx.db, service_id).await {
        match parts::resolve(&ctx.docker, &svc).await {
            Ok(p) if p.is_empty() => {
                out.blocked = Some("Este serviço não tem volumes: não há dados para salvar.".into())
            }
            Ok(_) => {}
            Err(e) => out.blocked = Some(e.to_string()),
        }
    }
    match s3::list(&r.cfg, &r.prefix).await {
        Ok(mut objs) => {
            objs.retain(|o| o.key.ends_with(".rpdata.age"));
            objs.sort_by(|a, b| b.key.cmp(&a.key));
            out.entries = objs
                .into_iter()
                .map(|o| BackupEntry {
                    key: o.key,
                    last_modified: o.last_modified,
                    size_bytes: o.size,
                })
                .collect();
        }
        Err(e) => out.list_error = Some(format!("{e:#}")),
    }
    out
}

// ── backup ──────────────────────────────────────────────────────────────────

/// Confere tudo o que dá para conferir na hora e dispara o backup em segundo plano.
pub async fn start_backup(
    ctx: DeployContext,
    service_id: String,
    stop: bool,
    keep: Option<u32>,
) -> Result<()> {
    let r = ready(&ctx, &service_id).await?;
    if let Some(run) = last_run(&ctx, &service_id).await
        && run.state == "running"
    {
        bail!("já há um backup ou uma restauração em andamento neste serviço");
    }
    let run = running("backup");
    save_run(&ctx, &service_id, &run).await;
    mark_start(&ctx, &service_id).await;
    tokio::spawn(async move {
        // uma coisa de cada vez por servidor: não roda junto com um build nem com outro backup
        let slot = ctx.deploy_queue.acquire_job_slot().await;
        let result = run_backup(&ctx, &service_id, &r, stop, keep).await;
        drop(slot);
        let mut done = finish(
            run,
            result.as_ref().map(|_| ()).map_err(|e| anyhow!("{e:#}")),
        );
        if let Ok((key, size)) = result {
            done.key = Some(key);
            done.size_bytes = Some(size);
        }
        match &done.error {
            None => info!(service_id, "backup de dados concluído"),
            Some(e) => warn!(service_id, error = %e, "backup de dados falhou"),
        }
        save_run(&ctx, &service_id, &done).await;
    });
    Ok(())
}

async fn run_backup(
    ctx: &DeployContext,
    service_id: &str,
    r: &Ready,
    stop: bool,
    keep: Option<u32>,
) -> Result<(String, u64)> {
    // sempre cifrado: `Some(true)`, nunca o padrão
    let exported = transfer::export(ctx.clone(), service_id, stop, Some(true)).await?;
    let size = tokio::fs::metadata(&exported.file).await?.len();
    let key = new_key(&r.prefix);
    s3::put_file(&r.cfg, &key, &exported.file).await?;
    drop(exported);
    // só depois de o novo estar no S3 é que os antigos saem
    if let Some(keep) = keep {
        match s3::list(&r.cfg, &r.prefix).await {
            Ok(objs) => {
                // a lista inclui o recém-enviado: ele é o mais novo, então fica entre os `keep`; e o filtro
                // final garante que nunca sai, nem com o relógio fora de hora
                let keys: Vec<String> = objs
                    .into_iter()
                    .map(|o| o.key)
                    .filter(|k| k.ends_with(".rpdata.age"))
                    .collect();
                for old in to_prune(&keys, keep).into_iter().filter(|k| k != &key) {
                    if let Err(e) = s3::delete(&r.cfg, &old).await {
                        warn!(service_id, key = %old, error = %e, "não consegui apagar um backup antigo");
                    }
                }
            }
            Err(e) => warn!(service_id, error = %e, "não consegui listar para aplicar a retenção"),
        }
    }
    Ok((key, size))
}

// ── restaurar ───────────────────────────────────────────────────────────────

pub async fn start_restore(ctx: DeployContext, service_id: String, key: String) -> Result<()> {
    let r = ready(&ctx, &service_id).await?;
    if !key_belongs(&r.prefix, &key) {
        bail!("esse backup não pertence a este serviço");
    }
    if let Some(run) = last_run(&ctx, &service_id).await
        && run.state == "running"
    {
        bail!("já há um backup ou uma restauração em andamento neste serviço");
    }
    let mut run = running("restore");
    run.key = Some(key.clone());
    save_run(&ctx, &service_id, &run).await;
    tokio::spawn(async move {
        let slot = ctx.deploy_queue.acquire_job_slot().await;
        let result = run_restore(&ctx, &service_id, &r, &key).await;
        drop(slot);
        let mut done = finish(
            run,
            result.as_ref().map(|_| ()).map_err(|e| anyhow!("{e:#}")),
        );
        if let Ok(checks) = result {
            done.checks = checks;
        }
        match &done.error {
            None => info!(service_id, "restauração de dados concluída"),
            Some(e) => warn!(service_id, error = %e, "restauração de dados falhou"),
        }
        save_run(&ctx, &service_id, &done).await;
    });
    Ok(())
}

async fn run_restore(
    ctx: &DeployContext,
    service_id: &str,
    r: &Ready,
    key: &str,
) -> Result<Vec<String>> {
    let work = transfer::Workdir::create(ctx)?;
    let file = work.path().join("baixado.rpdata.age");
    s3::get_to_file(&r.cfg, key, &file).await?;
    let report = transfer::import(ctx.clone(), service_id, &file, true).await?;
    Ok(report.checks)
}

// ── usado pela recuperação do servidor ──────────────────────────────────────

/// O backup mais recente do serviço no S3, se houver. `Err` quando não dá para consultar (sem destino,
/// sem chave de recuperação, S3 fora): quem chama decide se isso é "sem backup" ou uma falha.
pub async fn latest(ctx: &DeployContext, service_id: &str) -> Result<Option<BackupEntry>> {
    let r = ready(ctx, service_id).await?;
    let mut objs = s3::list(&r.cfg, &r.prefix).await?;
    objs.retain(|o| o.key.ends_with(".rpdata.age"));
    objs.sort_by(|a, b| b.key.cmp(&a.key));
    Ok(objs.into_iter().next().map(|o| BackupEntry {
        key: o.key,
        last_modified: o.last_modified,
        size_bytes: o.size,
    }))
}

/// Os primeiros bytes lidos de um backup para conferir a chave: cabe com folga o cabeçalho age (poucos
/// destinatários, ~200 bytes cada).
const HEAD_BYTES: usize = 16 * 1024;

/// **Este servidor consegue abrir o backup `key`?** (com a identidade dele ou, durante uma recuperação, com a
/// chave de recuperação informada). Lê só o início do objeto: confere a chave sem baixar o backup.
///
/// `Ok(false)` = o backup foi cifrado para **outra** chave (ex.: a chave de recuperação foi trocada depois
/// dele). `Err` = não deu para ler (S3 fora, objeto cortado): não prova nada, quem chama decide.
pub async fn opens_with_current_keys(
    ctx: &DeployContext,
    service_id: &str,
    key: &str,
) -> Result<bool> {
    let r = ready(ctx, service_id).await?;
    if !key_belongs(&r.prefix, key) {
        bail!("esse backup não pertence a este serviço");
    }
    let head = s3::get_head(&r.cfg, key, HEAD_BYTES).await?;
    ctx.secrets.can_open_file_head(&head)
}

/// Baixa um backup do serviço para `dest` (confere que a chave é da pasta do serviço).
pub async fn fetch(
    ctx: &DeployContext,
    service_id: &str,
    key: &str,
    dest: &std::path::Path,
) -> Result<()> {
    let r = ready(ctx, service_id).await?;
    if !key_belongs(&r.prefix, key) {
        bail!("esse backup não pertence a este serviço");
    }
    s3::get_to_file(&r.cfg, key, dest).await?;
    Ok(())
}

// ── política ────────────────────────────────────────────────────────────────

/// `Command::ServiceBackupPolicySet`: grava a política e devolve a listagem já com ela.
pub async fn policy_set(
    ctx: DeployContext,
    service_id: &str,
    policy: Option<shared::BackupPolicy>,
) -> Result<ServiceBackups> {
    let destination_ok = destination::view(&ctx.db, &ctx.secrets).await.configured;
    super::policy::set(
        &ctx.db,
        service_id,
        policy,
        destination_ok,
        ctx.secrets.has_recovery(),
    )?;
    Ok(list(ctx, service_id).await)
}

// ── apagar ──────────────────────────────────────────────────────────────────

pub async fn delete(ctx: DeployContext, service_id: &str, key: &str) -> Result<()> {
    let r = ready(&ctx, service_id).await?;
    if !key_belongs(&r.prefix, key) {
        bail!("esse backup não pertence a este serviço");
    }
    s3::delete(&r.cfg, key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasta_do_servico_com_e_sem_prefixo() {
        assert_eq!(
            service_prefix("rustploy/chiquitos", "p1", "s1"),
            "rustploy/chiquitos/p1/s1/"
        );
        assert_eq!(service_prefix("/a/", "p1", "s1"), "a/p1/s1/");
        assert_eq!(service_prefix("", "p1", "s1"), "p1/s1/");
    }

    #[test]
    fn so_aceita_chave_da_pasta_do_servico() {
        let p = "a/p1/s1/";
        assert!(key_belongs(p, "a/p1/s1/20261111-100000.rpdata.age"));
        assert!(!key_belongs(p, "a/p1/s2/20261111-100000.rpdata.age"));
        assert!(!key_belongs(p, "a/p1/s1/../s2/x.rpdata.age"));
        assert!(!key_belongs(p, "a/p1/s1/"));
        // prefixo parecido não vale (s10 não é s1)
        assert!(!key_belongs("a/p1/s1/", "a/p1/s10/x.rpdata.age"));
    }

    #[test]
    fn retencao_mantem_os_mais_novos_e_nunca_zera() {
        let keys: Vec<String> = [
            "20261101-000000",
            "20261103-000000",
            "20261102-000000",
            "20261104-000000",
        ]
        .iter()
        .map(|d| format!("p/s/{d}.rpdata.age"))
        .collect();
        let sai = to_prune(&keys, 2);
        assert_eq!(
            sai,
            vec![
                "p/s/20261102-000000.rpdata.age",
                "p/s/20261101-000000.rpdata.age"
            ]
        );
        // keep 0 vale 1: nunca apaga tudo
        assert_eq!(to_prune(&keys, 0).len(), 3);
        assert!(to_prune(&keys, 10).is_empty());
    }

    #[test]
    fn chave_nova_tem_a_pasta_e_a_extensao() {
        let k = new_key("a/p/s/");
        assert!(k.starts_with("a/p/s/") && k.ends_with(".rpdata.age"), "{k}");
    }
}
