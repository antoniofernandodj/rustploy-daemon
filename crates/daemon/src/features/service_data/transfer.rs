//! Mover os bytes: tirar o tar de cada volume (exportar) e devolvê-los (importar), com rede de
//! segurança. O volume é lido/gravado por um container descartável ([`super::HELPER_IMAGE`]).

use super::database;
use super::pack::{self, StagedPart};
use super::parts::{self, Part, Source};
use super::{HELPER_IMAGE, is_running};
use crate::features::deploy::DeployContext;
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing::{info, warn};

// ── uma operação por vez em cada serviço ────────────────────────────────────

static BUSY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Trava de "exportando/importando": solta ao sair de escopo.
struct Busy(String);

impl Busy {
    fn take(service_id: &str) -> Result<Self> {
        let set = BUSY.get_or_init(Default::default);
        let mut g = set.lock().unwrap_or_else(|e| e.into_inner());
        if !g.insert(service_id.to_string()) {
            bail!("já há uma exportação ou importação de dados em andamento neste serviço");
        }
        Ok(Self(service_id.to_string()))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(set) = BUSY.get() {
            set.lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.0);
        }
    }
}

/// Pasta de trabalho (apagada ao final) dentro da pasta de dados do daemon.
pub struct Workdir(PathBuf);

impl Workdir {
    pub fn create(state: &DeployContext) -> Result<Self> {
        let p = state
            .db_path
            .join("tmp")
            .join("service-data")
            .join(ulid::Ulid::new().to_string());
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ── o container descartável ─────────────────────────────────────────────────

fn mount_arg(source: &Source, ro: bool) -> String {
    let src = match source {
        Source::Named(n) | Source::Bind(n) => n,
        Source::Database(_) => unreachable!("banco não se monta como volume"),
    };
    format!("{src}:/data{}", if ro { ":ro" } else { "" })
}

/// Lê a parte para `out`: tar do volume ou dump do banco.
async fn read_part(source: &Source, out: &Path) -> Result<()> {
    match source {
        Source::Database(t) => database::dump_to(t, out).await,
        _ => tar_out(source, out).await,
    }
}

/// Substitui o conteúdo da parte pelo arquivo `from`.
async fn write_part(source: &Source, from: &Path) -> Result<()> {
    match source {
        Source::Database(t) => database::restore_from(t, from).await,
        _ => tar_in(source, from).await,
    }
}

/// Grava em `out` o tar do conteúdo do volume.
async fn tar_out(source: &Source, out: &Path) -> Result<()> {
    let mut cmd = crate::engine::cli();
    cmd.args(["run", "--rm", "-v", &mount_arg(source, true), HELPER_IMAGE])
        .args(["tar", "cf", "-", "-C", "/data", "."]);
    super::stream::run(cmd, None, Some(out))
        .await
        .map_err(|e| anyhow!("copiar o volume falhou: {e}"))
}

/// Esvazia o volume e extrai o tar `from` nele.
async fn tar_in(source: &Source, from: &Path) -> Result<()> {
    let mut cmd = crate::engine::cli();
    cmd.args([
        "run",
        "--rm",
        "-i",
        "-v",
        &mount_arg(source, false),
        HELPER_IMAGE,
    ])
    .args([
        "sh",
        "-c",
        "rm -rf /data/* /data/.[!.]* /data/..?* 2>/dev/null; tar xf - -C /data",
    ]);
    super::stream::run(cmd, Some(from), None)
        .await
        .map_err(|e| anyhow!("gravar no volume falhou: {e}"))
}

// ── parar e religar o serviço ───────────────────────────────────────────────

async fn stop_service(state: &DeployContext, service_id: &str) -> Result<()> {
    match crate::features::deploy::stop::handle(state.clone(), service_id.to_string()).await {
        shared::Response::Err { message, .. } => bail!("não consegui parar o serviço: {message}"),
        _ => Ok(()),
    }
}

/// Religa (um deploy novo, como o "Iniciar" de sempre). Falha só vira aviso: os dados já estão.
async fn start_service(state: &DeployContext, service_id: &str) {
    if let shared::Response::Err { message, .. } =
        crate::features::deploy::start::handle(state.clone(), service_id.to_string()).await
    {
        warn!(service_id, %message, "dados do serviço: não consegui religá-lo");
    }
}

async fn load(state: &DeployContext, service_id: &str) -> Result<shared::Service> {
    crate::store::services::get(&state.db, service_id)
        .await
        .map_err(|e| anyhow!(e.to_string()))?
        .ok_or_else(|| anyhow!("serviço não encontrado"))
}

// ── exportar ────────────────────────────────────────────────────────────────

pub struct Exported {
    /// Mantém a pasta viva até o download terminar.
    pub _workdir: Workdir,
    pub file: PathBuf,
    pub filename: String,
}

/// Gera o pacote do serviço. `stop`: parar o serviço durante a cópia (e religá-lo depois).
pub async fn export(
    state: DeployContext,
    service_id: &str,
    stop: bool,
    encrypt: Option<bool>,
) -> Result<Exported> {
    let _busy = Busy::take(service_id)?;
    let svc = load(&state, service_id).await?;
    let found = parts::resolve(&state.docker, &svc).await?;
    if found.is_empty() {
        bail!("este serviço não tem volumes: não há dados para exportar");
    }
    let workdir = Workdir::create(&state)?;
    let was_running = is_running(&svc.status);
    // banco: o dump lógico é consistente com o serviço no ar; só volume comum para
    let has_db = found
        .iter()
        .any(|p| matches!(p.source, Source::Database(_)));
    let stopped_by_us = stop && was_running && !has_db;
    if stopped_by_us {
        stop_service(&state, service_id).await?;
    }

    let staged = stage_all(&svc, &found, workdir.path()).await;

    // Religa antes de empacotar: o serviço não precisa esperar o tar externo.
    if stopped_by_us {
        start_service(&state, service_id).await;
    }
    let staged = staged?;

    let name = svc.spec.name.clone();
    let out = workdir.path().join("pacote.rpdata");
    let out2 = out.clone();
    tokio::task::spawn_blocking(move || pack::write_pack(&name, &staged, &out2)).await??;

    // Cifra por padrão quando há chave de recuperação (só assim o pacote abre num servidor
    // restaurado); `encrypt=false` entrega em claro, `true` cifra mesmo sem recuperação.
    let encrypt = encrypt.unwrap_or_else(|| state.secrets.has_recovery());
    let (file, ext) = if encrypt {
        let enc = workdir.path().join("pacote.rpdata.age");
        let (secrets, src, dst) = (state.secrets.clone(), out.clone(), enc.clone());
        tokio::task::spawn_blocking(move || secrets.encrypt_file(&src, &dst)).await??;
        let _ = std::fs::remove_file(&out);
        (enc, "rpdata.age")
    } else {
        (out, "rpdata")
    };

    let day = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    info!(service_id, encrypt, "dados do serviço exportados");
    Ok(Exported {
        _workdir: workdir,
        file,
        filename: format!("{}-{day}.{ext}", shared::normalize_name(&svc.spec.name)),
    })
}

async fn stage_all(svc: &shared::Service, found: &[Part], dir: &Path) -> Result<Vec<StagedPart>> {
    let mut staged = Vec::new();
    for (i, p) in found.iter().enumerate() {
        let path = dir.join(format!("{i}.raw"));
        let mut sp = StagedPart::volume(&p.name, path.clone());
        if let Source::Database(t) = &p.source {
            // contagens antes do dump: servem de conferência (escritas no meio explicam diferenças)
            sp.counts = database::counts(t).await.unwrap_or(None);
            sp.kind = "database".into();
            sp.engine = Some(t.engine.kind_id().into());
            sp.version = database::version_of(svc);
        }
        read_part(&p.source, &path).await?;
        staged.push(sp);
    }
    Ok(staged)
}

// ── importar ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, PartialEq)]
pub struct ImportReport {
    /// Partes do pacote que serão (ou foram) substituídas.
    pub parts: Vec<String>,
    /// `false` = só conferiu (sem `confirm`); `true` = aplicou.
    pub applied: bool,
    pub service_was_running: bool,
    /// Conferências feitas depois de aplicar (ex.: diferenças de contagem por tabela).
    pub checks: Vec<String>,
}

/// Partes do pacote × partes do serviço: devolve a lista a substituir, ou a causa da recusa.
pub fn match_parts<'a>(
    manifest: &'a pack::Manifest,
    found: &'a [Part],
    dest_image: Option<&str>,
) -> Result<Vec<(&'a pack::ManifestPart, &'a Part)>> {
    let mut out = Vec::new();
    for mp in &manifest.parts {
        let part = found.iter().find(|p| p.name == mp.name).ok_or_else(|| {
            anyhow!(
                "o pacote traz a parte `{}`, que este serviço não tem (as dele: {})",
                mp.name,
                found
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if let Source::Database(t) = &part.source {
            database::compatible(
                t.engine,
                dest_image,
                mp.engine.as_deref().unwrap_or(""),
                mp.version.as_deref(),
            )?;
        } else if mp.kind != "volume" {
            bail!(
                "a parte `{}` é um dump de banco e este serviço guarda volumes",
                mp.name
            );
        }
        out.push((mp, part));
    }
    if out.is_empty() {
        bail!("o pacote não traz nenhuma parte");
    }
    Ok(out)
}

/// Importa o pacote em `pack_file`. Sem `confirm`, só confere e diz o que seria substituído.
pub async fn import(
    state: DeployContext,
    service_id: &str,
    pack_file: &Path,
    confirm: bool,
) -> Result<ImportReport> {
    import_with(state, service_id, pack_file, confirm, false).await
}

/// Como [`import`]. `precreate` (recuperação de um servidor): as partes de **volume** que o pacote traz e
/// o serviço ainda não tem (porque nunca subiu aqui) são criadas a partir do spec do serviço
/// ([`parts::synthesize`]); as de banco continuam exigindo o banco no ar.
pub async fn import_with(
    state: DeployContext,
    service_id: &str,
    pack_file: &Path,
    confirm: bool,
    precreate: bool,
) -> Result<ImportReport> {
    let _busy = Busy::take(service_id)?;
    let svc = load(&state, service_id).await?;
    let mut found = match parts::resolve(&state.docker, &svc).await {
        Ok(f) => f,
        // sem o banco no ar não há partes a resolver; na recuperação o volume ainda pode ser criado
        Err(_) if precreate => Vec::new(),
        Err(e) => return Err(e),
    };
    let was_running = is_running(&svc.status);

    // Pacote cifrado (age): abre para um arquivo temporário com a identidade deste servidor.
    let _opened;
    let mut pack_file = pack_file.to_path_buf();
    if crate::platform::age_format::is_age_file(&pack_file) {
        let w = Workdir::create(&state)?;
        let plain = w.path().join("aberto.rpdata");
        let (secrets, src, dst) = (state.secrets.clone(), pack_file.clone(), plain.clone());
        tokio::task::spawn_blocking(move || secrets.decrypt_file(&src, &dst))
            .await?
            .map_err(|e| {
                anyhow!(
                    "o pacote está cifrado e este servidor não o abre ({e}). Importe num servidor \
                     com a mesma identidade (ou a chave de recuperação) ou exporte sem cifrar."
                )
            })?;
        pack_file = plain;
        _opened = w;
    }
    let pack_file = pack_file.as_path();
    let pf = pack_file.to_path_buf();
    let manifest = tokio::task::spawn_blocking(move || pack::read_manifest(&pf)).await??;
    if precreate {
        for mp in manifest.parts.iter().filter(|m| m.kind == "volume") {
            if found.iter().any(|p| p.name == mp.name) {
                continue;
            }
            if let Some(part) = parts::synthesize(&svc, &mp.name) {
                if let Source::Bind(dir) = &part.source {
                    let _ = std::fs::create_dir_all(dir);
                }
                found.push(part);
            }
        }
    }
    let plan = match_parts(&manifest, &found, database::version_of(&svc).as_deref())?;
    let names: Vec<String> = plan.iter().map(|(m, _)| m.name.clone()).collect();
    if !confirm {
        return Ok(ImportReport {
            parts: names,
            applied: false,
            service_was_running: was_running,
            checks: vec![],
        });
    }

    // Tudo conferido e extraído ANTES de tocar em qualquer volume.
    let work = Workdir::create(&state)?;
    let incoming = work.path().join("in");
    std::fs::create_dir(&incoming)?;
    let (pf, inc) = (pack_file.to_path_buf(), incoming.clone());
    tokio::task::spawn_blocking(move || pack::extract_verified(&pf, &inc)).await??;

    // banco: o restore precisa dele no ar; só volume comum para durante a troca
    let has_db = plan
        .iter()
        .any(|(_, p)| matches!(p.source, Source::Database(_)));
    let stop = was_running && !has_db;
    if stop {
        stop_service(&state, service_id).await?;
    }
    let result = apply(&plan, &incoming, work.path()).await;
    if stop {
        start_service(&state, service_id).await;
    }
    if let Err((e, keep)) = result {
        if keep {
            // o backup é a única cópia dos dados antigos: a pasta fica no disco
            std::mem::forget(work);
        }
        return Err(e);
    }
    info!(
        service_id,
        parts = names.len(),
        "dados do serviço importados"
    );
    let mut checks = Vec::new();
    for (mp, part) in &plan {
        if let (Source::Database(t), Some(before)) = (&part.source, &mp.counts) {
            match database::counts(t).await {
                Ok(Some(now)) => {
                    let d = database::diff_counts(before, &now);
                    if d.is_empty() {
                        checks.push(format!(
                            "`{}`: {} tabelas conferidas, contagens iguais às do export",
                            mp.name,
                            before.lines().count()
                        ));
                    } else {
                        checks.push(format!(
                            "`{}`: contagens diferentes das do export (esperado se o banco de \
                             origem recebia escritas durante o dump): {}",
                            mp.name,
                            d.join("; ")
                        ));
                    }
                }
                Ok(None) => {}
                Err(e) => checks.push(format!(
                    "`{}`: não consegui conferir as contagens: {e}",
                    mp.name
                )),
            }
        }
    }
    Ok(ImportReport {
        parts: names,
        applied: true,
        service_was_running: was_running,
        checks,
    })
}

/// Substitui as partes uma a uma. Antes de cada uma guarda o conteúdo atual; se qualquer coisa
/// falhar, devolve as já trocadas (e a que falhou) ao que eram.
async fn apply(
    plan: &[(&pack::ManifestPart, &Part)],
    incoming: &Path,
    work: &Path,
) -> std::result::Result<(), (anyhow::Error, bool)> {
    let mut done: Vec<(&Part, PathBuf)> = Vec::new();
    for (i, (mp, part)) in plan.iter().enumerate() {
        let backup = work.join(format!("antes-{i}.tar"));
        let step = async {
            read_part(&part.source, &backup).await?;
            // registrada antes de gravar: se o `write_part` morrer no meio, o rollback a cobre
            done.push((part, backup.clone()));
            write_part(&part.source, &incoming.join(&mp.file)).await
        }
        .await;
        if let Err(e) = step {
            let mut msg = format!("importar `{}` falhou: {e}", mp.name);
            let mut keep = false;
            for (p, b) in done.iter().rev() {
                if let Err(r) = write_part(&p.source, b).await {
                    msg.push_str(&format!(
                        "; ATENÇÃO: não consegui devolver `{}` ao estado anterior ({r}); \
                         a cópia está em {}",
                        p.name,
                        b.display()
                    ));
                    keep = true;
                } else {
                    msg.push_str(&format!("; `{}` voltou ao que era", p.name));
                }
            }
            return Err((anyhow!(msg), keep));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::service_data::HELPER_IMAGE;
    use crate::features::service_data::pack::{Manifest, ManifestPart};

    fn m(names: &[&str]) -> Manifest {
        Manifest {
            format: 1,
            rustploy_version: "x".into(),
            service_name: "s".into(),
            created_at: String::new(),
            parts: names
                .iter()
                .enumerate()
                .map(|(i, n)| ManifestPart {
                    name: (*n).into(),
                    file: format!("vol{i}.tar"),
                    size: 0,
                    sha256: String::new(),
                    kind: "volume".into(),
                    engine: None,
                    version: None,
                    counts: None,
                })
                .collect(),
        }
    }

    fn part(n: &str) -> Part {
        Part {
            name: n.into(),
            source: Source::Named(format!("rp_{n}")),
        }
    }

    #[test]
    fn casa_pelo_nome_logico() {
        let found = vec![part("a"), part("b")];
        let man = m(&["b"]);
        let r = match_parts(&man, &found, None).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1.name, "b");
    }

    #[test]
    fn parte_desconhecida_e_recusada_com_a_lista() {
        let found = vec![part("a")];
        let err = match_parts(&m(&["z"]), &found, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`z`") && err.contains("a"), "{err}");
        assert!(match_parts(&m(&[]), &found, None).is_err());
    }

    /// Precisa de Docker/Podman e de rede (puxa o busybox):
    /// `RP_TEST_SOCKET=/run/user/1000/podman/podman.sock cargo test -p rustploy volume_vai -- --ignored`
    /// (sem `RP_TEST_SOCKET`, `/var/run/docker.sock`).
    #[tokio::test]
    #[ignore = "precisa de um engine Docker/Podman rodando"]
    async fn volume_vai_e_volta_pelo_tar() {
        let socket =
            std::env::var("RP_TEST_SOCKET").unwrap_or_else(|_| "/var/run/docker.sock".into());
        // grava o engine (Docker × Podman) que `crate::engine::cli()` usa
        crate::engine::DockerClient::connect(&socket).expect("engine");
        let name = format!("rp_test_{}", ulid::Ulid::new().to_string().to_lowercase());
        let src = Source::Named(name.clone());
        let d = tempfile::tempdir().unwrap();
        async fn run(args: &[&str]) -> std::process::Output {
            crate::engine::cli().args(args).output().await.unwrap()
        }
        let vol = format!("{name}:/data");
        // enche o volume
        let seed = run(&[
            "run",
            "--rm",
            "-v",
            &vol,
            HELPER_IMAGE,
            "sh",
            "-c",
            "mkdir -p /data/a && echo oi > /data/a/f && echo oculto > /data/.h",
        ])
        .await;
        assert!(
            seed.status.success(),
            "{}",
            String::from_utf8_lossy(&seed.stderr)
        );

        let t1 = d.path().join("1.tar");
        tar_out(&src, &t1).await.unwrap();
        // estraga e devolve
        let wipe = run(&[
            "run",
            "--rm",
            "-v",
            &vol,
            HELPER_IMAGE,
            "sh",
            "-c",
            "rm -rf /data/* /data/.h; echo lixo > /data/outro",
        ])
        .await;
        assert!(wipe.status.success());
        tar_in(&src, &t1).await.unwrap();

        let ls = run(&[
            "run",
            "--rm",
            "-v",
            &vol,
            HELPER_IMAGE,
            "sh",
            "-c",
            "cat /data/a/f /data/.h; ls /data",
        ])
        .await;
        let out = String::from_utf8_lossy(&ls.stdout).to_string();
        let _ = run(&["volume", "rm", "-f", &name]).await;
        assert!(out.contains("oi") && out.contains("oculto"), "{out}");
        assert!(
            !out.contains("outro"),
            "o conteúdo antigo devia sumir: {out}"
        );
    }

    #[test]
    fn trava_de_um_por_vez() {
        let a = Busy::take("svc_teste_trava").unwrap();
        assert!(Busy::take("svc_teste_trava").is_err());
        drop(a);
        assert!(Busy::take("svc_teste_trava").is_ok());
    }
}
