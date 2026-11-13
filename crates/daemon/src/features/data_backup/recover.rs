//! **Recuperar este servidor** (`docs/plano-backup-de-dados-para-s3.md`, Fase 4): num servidor limpo, traz a
//! infra do git, recifra os segredos para este servidor e restaura os dados do S3, subindo cada serviço na
//! ordem das dependências.
//!
//! Dois passos, para a pessoa ver o que foi achado antes de qualquer dado ser tocado:
//! 1. [`start`]: restaura a infra (sem subir nada, sem tocar em dado) e lista os backups por serviço;
//! 2. [`run`]: restaura os dados e sobe, em segundo plano (volumes **antes** de subir, bancos **depois**:
//!    o dump precisa do banco no ar). Reaproveita o `orchestrate` do autostart: ordem, dependência
//!    bloqueada e novas tentativas.
//!
//! A chave **privada** de recuperação fica só na memória (`SecretsManager::begin_recovery`) e é descartada ao
//! fim, ao cancelar ou depois de 1 hora parada. Só a pública vai para o `config.toml`.

use super::service_backup;
use crate::features::deploy::DeployContext;
use crate::features::deploy::autostart::{self, Bringer, DockerBringer, Policy, Progress};
use crate::features::service_data::{database, transfer};
use crate::features::state_git::StateGit;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use shared::{RecoverService, ServerRecoverInfo, Service};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tracing::{info, warn};

/// Parado em "pronto" por mais que isto, a chave sai da memória.
const IDLE_TTL: Duration = Duration::from_secs(3600);

static INFO: OnceLock<Mutex<ServerRecoverInfo>> = OnceLock::new();

fn cell() -> &'static Mutex<ServerRecoverInfo> {
    INFO.get_or_init(|| {
        Mutex::new(ServerRecoverInfo {
            phase: "idle".into(),
            ..Default::default()
        })
    })
}

pub fn status() -> ServerRecoverInfo {
    cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn update(f: impl FnOnce(&mut ServerRecoverInfo)) {
    f(&mut cell().lock().unwrap_or_else(|e| e.into_inner()));
}

fn set_step(service_id: &str, step: &str, error: Option<String>) {
    update(|i| {
        if let Some(s) = i.services.iter_mut().find(|s| s.service_id == service_id) {
            s.step = step.into();
            s.error = error;
        }
    });
}

fn is_database(svc: &Service) -> bool {
    database::engine_of(svc).is_some()
}

/// `true` durante a restauração da infra do [`start`] (a fase ainda é "idle" nessa janela).
static STARTING: AtomicBool = AtomicBool::new(false);

/// A infra deste servidor veio de uma recuperação cujos **dados ainda não foram tocados** (liga ao fim da
/// restauração da infra; desliga quando o `run` começa). Enquanto ligado, dá para chamar `start` de novo
/// com outra chave: o servidor "já tem projetos", mas são os desta recuperação. Só em memória: um reinício
/// do daemon o esquece (aí é um servidor com projetos como outro qualquer).
static INFRA_PENDING: AtomicBool = AtomicBool::new(false);

/// Liga [`STARTING`] e desliga em qualquer saída de `start` (erro, `?` ou sucesso).
struct StartingGuard;

impl StartingGuard {
    fn new() -> Self {
        STARTING.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for StartingGuard {
    fn drop(&mut self) {
        STARTING.store(false, Ordering::SeqCst);
    }
}

/// Há uma recuperação em curso (restaurando a infra, esperando a confirmação ou restaurando dados)?
///
/// O agendador de backups **não pode rodar** enquanto isto for verdade. O `last_start` de cada serviço vive
/// no SQL local e não vai para o git, então num servidor recuperado ele vem vazio e todo serviço cuja
/// política é mais velha que um período aparece como "vencido" no primeiro minuto. Um backup nessa hora
/// grava o serviço **ainda vazio** como o mais recente, e a próxima recuperação restauraria o vazio.
pub fn in_progress() -> bool {
    STARTING.load(Ordering::SeqCst) || matches!(status().phase.as_str(), "ready" | "running")
}

// ── passo 1: a infra ────────────────────────────────────────────────────────

/// Restaura a infra e monta o resumo. `provider_id` é o provider git criado com o token (`None` num remoto
/// local).
pub async fn start(
    ctx: DeployContext,
    state_git: Arc<StateGit>,
    remote: String,
    provider_id: Option<String>,
    branch: Option<String>,
    recovery_key: String,
) -> Result<ServerRecoverInfo> {
    if matches!(status().phase.as_str(), "running") {
        bail!("já há uma recuperação em andamento");
    }
    // Os projetos que já estão aqui podem ser os desta mesma recuperação, ainda sem nenhum dado restaurado
    // (a pessoa informou a chave errada, cancelou ou quer tentar de novo): nesse caso deixa informar outra
    // chave em vez de obrigar a destruir o servidor. Depois do `run`, não.
    let resuming = INFRA_PENDING.load(Ordering::SeqCst);
    if !resuming && !ctx.db.infra().snapshot().projects.is_empty() {
        bail!(
            "este servidor já tem projetos: a recuperação só funciona num servidor **limpo** (para não \
             sobrescrever nada). Use um servidor novo, ou apague os projetos daqui antes."
        );
    }
    // daqui até o retorno o agendador de backups fica parado (ver `in_progress`)
    let _starting = StartingGuard::new();
    let public = ctx.secrets.begin_recovery(&recovery_key)?;
    state_git.set_hold_restore(true);
    let restored = restore_infra(&ctx, &state_git, remote, provider_id, branch).await;
    state_git.set_hold_restore(false);
    if let Err(e) = restored {
        ctx.secrets.end_recovery();
        return Err(e);
    }
    INFRA_PENDING.store(true, Ordering::SeqCst);

    let info = summarize(&ctx, public).await;
    update(|i| *i = info.clone());
    // esquece a chave se ninguém confirmar
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_TTL).await;
        if status().phase == "ready" {
            warn!("recuperação parada por 1 hora: a chave de recuperação saiu da memória");
            ctx.secrets.end_recovery();
            update(|i| {
                i.phase = "idle".into();
                i.message = Some(
                    "a chave de recuperação foi esquecida por inatividade; comece de novo".into(),
                );
            });
        }
    });
    Ok(info)
}

async fn restore_infra(
    ctx: &DeployContext,
    state_git: &Arc<StateGit>,
    remote: String,
    provider_id: Option<String>,
    branch: Option<String>,
) -> Result<()> {
    state_git
        .configure("sync", Some(remote), branch, provider_id)
        .await
        .map_err(|e| anyhow!("restaurar a infra do git falhou: {e}"))?;
    if ctx.db.infra().snapshot().projects.is_empty() {
        bail!(
            "o repositório não trouxe nenhum projeto (vazio, branch errada ou outro repositório). \
             Nada foi restaurado."
        );
    }
    // Os segredos vieram cifrados para o servidor antigo: recifra para este servidor + a chave pública de
    // recuperação. Daqui em diante a chave privada não é mais necessária para a infra.
    let n = crate::store::infra::rekey::rekey_all(ctx.db.infra(), &ctx.secrets)
        .map_err(|e| anyhow!("recifrar os segredos falhou: {e:#}"))?;
    info!(
        valores = n,
        "recuperação: segredos recifrados para este servidor"
    );
    // empurra a infra recifrada (melhor esforço: o ciclo normal faz isso de qualquer jeito)
    let _ = state_git.sync_now().await;
    Ok(())
}

/// O que foi achado: projetos, serviços e o backup mais recente de cada um.
async fn summarize(ctx: &DeployContext, public: String) -> ServerRecoverInfo {
    let snap = ctx.db.infra().snapshot();
    let destination = super::destination::view(&ctx.db, &ctx.secrets).await;
    let mut info = ServerRecoverInfo {
        phase: "ready".into(),
        projects: snap.projects.len() as u32,
        destination_configured: destination.configured,
        recovery_public_key: Some(public),
        ..Default::default()
    };
    let services = crate::store::services::list_all(&ctx.db)
        .await
        .unwrap_or_default();
    for svc in services {
        let project = snap
            .projects
            .iter()
            .find(|p| p.id == svc.spec.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        // Se a chave informada não abrir o backup mais recente, a pessoa precisa saber AGORA (com nada
        // iniciado e nenhum dado tocado), não no meio da restauração, quando o serviço já falhou.
        let mut key_error: Option<String> = None;
        let latest = if destination.configured {
            match service_backup::latest(ctx, &svc.id).await {
                Ok(Some(entry)) => {
                    match service_backup::opens_with_current_keys(ctx, &svc.id, &entry.key).await {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!(
                                service = %svc.spec.name, service_id = %svc.id, key = %entry.key,
                                "recuperação: a chave informada não abre o backup mais recente"
                            );
                            key_error = Some(
                                "a chave de recuperação informada não abre o backup mais recente \
                                 deste serviço (ele foi cifrado para outra chave)"
                                    .into(),
                            );
                        }
                        // não deu para ler o cabeçalho: não prova nada, a restauração tenta de qualquer jeito
                        Err(e) => warn!(
                            service = %svc.spec.name, service_id = %svc.id, error = %format!("{e:#}"),
                            "recuperação: não consegui conferir a chave contra o backup"
                        ),
                    }
                    Some(entry.last_modified)
                }
                Ok(None) => None,
                Err(e) => {
                    info.destination_error
                        .get_or_insert_with(|| format!("{e:#}"));
                    None
                }
            }
        } else {
            None
        };
        info.services.push(RecoverService {
            service_id: svc.id.clone(),
            name: svc.spec.name.clone(),
            project,
            kind: if is_database(&svc) {
                "database"
            } else {
                "volume"
            }
            .into(),
            latest_backup: latest,
            step: "pending".into(),
            error: key_error,
        });
    }
    info.services.sort_by(|a, b| {
        (a.project.as_str(), a.name.as_str()).cmp(&(b.project.as_str(), b.name.as_str()))
    });
    // Os tokens de login dos providers git nunca vão ao git (ficam só no SQL do servidor antigo): os
    // providers voltam sem credencial e todo clone privado falha depois (jobs/serviços `git_source`).
    if let Ok(providers) = crate::store::git_providers::list(&ctx.db).await {
        let names: Vec<String> = providers
            .iter()
            .filter(|p| p.access_token_enc.is_none())
            .map(|p| p.name.clone())
            .collect();
        if !names.is_empty() {
            info.message = Some(format!(
                "reconecte {} em Settings → Git providers (token/PAT ou OAuth): os tokens de login \
                 não vão para o git, então {} sem credencial e os clones de repositório privado \
                 (jobs de pré-deploy, serviços Git) vão falhar até lá",
                names.join(", "),
                if names.len() == 1 {
                    "ele voltou"
                } else {
                    "eles voltaram"
                },
            ));
        }
    }
    if !destination.configured {
        info.destination_error.get_or_insert_with(|| {
            "o repositório não trouxe o destino S3: os serviços sobem vazios".into()
        });
    }
    let blind: Vec<String> = info
        .services
        .iter()
        .filter(|s| s.error.is_some())
        .map(|s| s.name.clone())
        .collect();
    if !blind.is_empty() {
        let warning = format!(
            "A chave de recuperação informada ({}) NÃO abre os backups de: {}. Eles foram cifrados para \
             outra chave (a chave de recuperação foi trocada depois deles?). Informe a que estava \
             configurada no servidor quando os backups foram feitos. Enquanto isso a restauração dos \
             dados não será iniciada. Se essa chave se perdeu, cancele e suba os serviços à mão (vazios).",
            info.recovery_public_key.as_deref().unwrap_or("?"),
            blind.join(", "),
        );
        info.message = Some(match info.message.take() {
            Some(other) => format!("{warning}\n\n{other}"),
            None => warning,
        });
    }
    info
}

// ── passo 2: os dados e o religamento ───────────────────────────────────────

/// Dispara a restauração dos dados e o religamento, em segundo plano.
pub async fn run(ctx: DeployContext) -> Result<()> {
    if status().phase != "ready" {
        bail!("não há uma recuperação pronta para confirmar (comece por `ServerRecoverStart`)");
    }
    // Chave que não abre os backups: restaurar agora só produziria serviços que falham ou sobem vazios.
    // A pessoa vê o motivo no resumo e pode informar outra chave (ver `INFRA_PENDING`).
    let blind: Vec<String> = status()
        .services
        .iter()
        .filter(|s| s.step == "pending" && s.error.is_some())
        .map(|s| s.name.clone())
        .collect();
    if !blind.is_empty() {
        bail!(
            "a chave de recuperação informada não abre os backups de: {}. Informe a chave que estava \
             configurada quando eles foram feitos (cancele e comece de novo com ela).",
            blind.join(", ")
        );
    }
    // daqui os dados são tocados: não dá mais para recomeçar com outra chave
    INFRA_PENDING.store(false, Ordering::SeqCst);
    update(|i| {
        i.phase = "running".into();
        i.message = None;
    });
    tokio::spawn(async move {
        let result = run_inner(&ctx).await;
        ctx.secrets.end_recovery();
        update(|i| match &result {
            Ok(()) => {
                i.phase = "done".into();
                i.message = None;
            }
            Err(e) => {
                i.phase = "failed".into();
                i.message = Some(format!("{e:#}"));
            }
        });
    });
    Ok(())
}

async fn run_inner(ctx: &DeployContext) -> Result<()> {
    let services = crate::store::services::list_all(&ctx.db).await?;
    // a intenção é "no ar": o autostart e o `DockerBringer` partem dela
    for svc in services.iter().filter(|s| s.spec.autostart) {
        crate::store::services::set_desired_state(
            &ctx.db,
            &svc.id,
            crate::store::services::DesiredState::Running,
        )
        .await?;
    }
    let candidates = autostart::boot_candidates(&ctx.db).await?;
    let access = crate::store::shared_access::all(&ctx.db)
        .await
        .unwrap_or_default();
    let implicit = autostart::implicit_edges(&access, &services);
    let policy = Policy::default();
    let bringer = RecoveryBringer {
        inner: DockerBringer {
            ctx: ctx.clone(),
            policy: policy.clone(),
        },
        ctx: ctx.clone(),
        restored: Mutex::new(HashSet::new()),
    };
    info!(
        servicos = candidates.len(),
        "recuperação: restaurando dados e subindo na ordem das dependências"
    );
    let out = autostart::orchestrate(candidates, &implicit, &bringer, &policy).await;
    if out.all_ready() {
        info!(prontos = out.ready.len(), "recuperação: tudo no ar");
        Ok(())
    } else {
        bail!(
            "{} serviço(s) ficaram de fora (veja o motivo em cada um); os dados dos que subiram já voltaram",
            out.failed.len() + out.blocked.len()
        )
    }
}

/// Sobe cada serviço como o autostart, mas restaurando os dados no momento certo.
struct RecoveryBringer {
    inner: DockerBringer,
    ctx: DeployContext,
    /// Serviços cujos dados já voltaram: uma nova tentativa do `orchestrate` não restaura de novo.
    restored: Mutex<HashSet<String>>,
}

impl RecoveryBringer {
    /// O backup mais recente do serviço. Sem destino configurado ou sem backup = `None` (sobe vazio);
    /// o S3 não responder é **falha** (subir vazio e começar a gravar por cima seria pior que esperar).
    async fn find_latest(&self, svc: &Service) -> Result<Option<String>, String> {
        let dest = super::destination::view(&self.ctx.db, &self.ctx.secrets).await;
        if !dest.configured {
            return Ok(None);
        }
        let latest = service_backup::latest(&self.ctx, &svc.id)
            .await
            .map(|o| o.map(|e| e.key))
            .map_err(|e| format!("não consegui consultar os backups no S3: {e:#}"))?;
        match &latest {
            Some(key) => info!(
                service = %svc.spec.name, service_id = %svc.id, key = %key,
                "recuperação: backup mais recente achado"
            ),
            None => info!(
                service = %svc.spec.name, service_id = %svc.id,
                "recuperação: sem backup no S3, o serviço sobe vazio"
            ),
        }
        Ok(latest)
    }

    async fn restore(&self, svc: &Service, key: &str, database: bool) -> Result<(), String> {
        let work = transfer::Workdir::create(&self.ctx).map_err(|e| e.to_string())?;
        let file = work.path().join("backup.rpdata.age");
        info!(
            service = %svc.spec.name, service_id = %svc.id, database,
            "recuperação: baixando o backup"
        );
        service_backup::fetch(&self.ctx, &svc.id, key, &file)
            .await
            .map_err(|e| format!("baixar o backup falhou: {e:#}"))?;
        // volume: cria o que ainda não existe (`precreate`); banco: já está no ar, usa o dele
        info!(
            service = %svc.spec.name, service_id = %svc.id, database,
            "recuperação: restaurando os dados"
        );
        transfer::import_with(self.ctx.clone(), &svc.id, &file, true, !database)
            .await
            .map(|_| ())
            .map_err(|e| format!("restaurar os dados falhou: {e:#}"))?;
        info!(
            service = %svc.spec.name, service_id = %svc.id,
            "recuperação: dados restaurados"
        );
        Ok(())
    }

    fn already(&self, id: &str) -> bool {
        self.restored
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }

    fn mark(&self, id: &str) {
        self.restored
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string());
    }
}

#[async_trait]
impl Bringer for RecoveryBringer {
    async fn bring_up(&self, svc: &Service) -> Result<(), String> {
        let result = self.bring_up_steps(svc).await;
        // O motivo só vivia na memória (e na tela): sem isto o journal não dizia por que um serviço
        // ficou de fora, nem se os dados chegaram a voltar.
        if let Err(e) = &result {
            warn!(
                service = %svc.spec.name, service_id = %svc.id, error = %e,
                "recuperação: falhou ao restaurar/subir (nova tentativa conforme a política)"
            );
        }
        result
    }

    async fn report(&self, svc: &Service, progress: Progress) {
        match progress {
            Progress::Starting => set_step(&svc.id, "starting", None),
            Progress::Ready => {
                info!(service = %svc.spec.name, service_id = %svc.id, "recuperação: serviço pronto");
                set_step(&svc.id, "ready", None)
            }
            Progress::Failed(e) => {
                warn!(service = %svc.spec.name, service_id = %svc.id, error = %e, "recuperação: serviço falhou");
                set_step(&svc.id, "failed", Some(e))
            }
            Progress::Blocked(e) => {
                warn!(service = %svc.spec.name, service_id = %svc.id, error = %e, "recuperação: serviço bloqueado");
                set_step(&svc.id, "blocked", Some(e))
            }
        }
    }
}

impl RecoveryBringer {
    /// Os passos de [`Bringer::bring_up`]: achar o backup, restaurar (volume antes, banco depois) e subir.
    async fn bring_up_steps(&self, svc: &Service) -> Result<(), String> {
        let database = is_database(svc);
        let key = if self.already(&svc.id) {
            None
        } else {
            self.find_latest(svc).await?
        };
        // volume: os dados entram ANTES de o serviço subir
        if let (Some(key), false) = (&key, database) {
            set_step(&svc.id, "restoring", None);
            self.restore(svc, key, false).await?;
            self.mark(&svc.id);
        }
        self.inner.bring_up(svc).await?;
        // banco: o dump entra DEPOIS, com o banco no ar
        if let (Some(key), true) = (&key, database) {
            set_step(&svc.id, "restoring", None);
            self.restore(svc, key, true).await?;
            self.mark(&svc.id);
        }
        Ok(())
    }
}

// ── cancelar ────────────────────────────────────────────────────────────────

/// Desiste: a chave sai da memória. A infra restaurada continua no servidor.
pub fn cancel(ctx: &DeployContext) -> ServerRecoverInfo {
    if status().phase == "running" {
        return status();
    }
    ctx.secrets.end_recovery();
    update(|i| {
        *i = ServerRecoverInfo {
            phase: "idle".into(),
            message: Some(
                "recuperação cancelada; a infra restaurada continua neste servidor".into(),
            ),
            ..Default::default()
        }
    });
    status()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O agendador de backups só pode voltar depois de `start` terminar, seja por onde sair.
    #[test]
    fn a_guarda_do_start_liga_e_sempre_desliga() {
        assert!(!STARTING.load(Ordering::SeqCst));
        {
            let _g = StartingGuard::new();
            assert!(STARTING.load(Ordering::SeqCst));
            assert!(in_progress());
        }
        assert!(!STARTING.load(Ordering::SeqCst));
    }
}
