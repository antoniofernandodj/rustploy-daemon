//! Armazenamento do **log de runtime** dos containers (plano: `docs/plano-log-de-runtime-persistido.md`).
//!
//! Uma pasta **por execução** (= por container): `<dados>/logs/<service_id>/<container curto>/`, com
//! *segmentos* `000001.jsonl`, `000002.jsonl`… e um `meta.json` (deploy, imagem, primeiro/último
//! instante, linhas, bytes). Cada linha de segmento é um JSON `{"t","c","s","m"}` (instante,
//! container curto, `o`/`e`, texto). Um segmento fecha ao passar de [`SEGMENT_BYTES`].
//!
//! Listar as execuções é ler as pastas e os `meta.json`; excluir uma é apagar a pasta. A retenção
//! (`retention_days`, `max_bytes_per_service`) apaga **execuções inteiras**, da mais velha para a mais
//! nova, e nunca a mais recente do serviço (a que está rodando); se só ela passa do teto, a rotação de
//! segmentos roda **dentro** da pasta dela e a execução fica `partial`.
//!
//! Um **único escritor** (thread própria) recebe as linhas por um canal e grava em lote, para o caminho
//! quente do daemon nunca esperar o disco. É ele também quem descarta linhas repetidas: guarda o último
//! instante gravado por container, então reconectar um stream (ou capturar o que sobrou antes de
//! destruir o container) não duplica nada.
//!
//! Até a v0.1.40 os segmentos ficavam direto na pasta do serviço, com os containers misturados:
//! [`migrate_legacy`] redistribui essas linhas por container no primeiro boot.
//!
//! O armazenamento é processo-global ([`init`]/[`global`]): o deploy, o streaming e os handlers precisam
//! dele e passá-lo por todos os construtores não compensava. Sem [`init`] (testes, `enabled = false`),
//! [`global`] devolve `None` e tudo vira no-op.

use crate::platform::config::LogsConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::protocol::{LogHistoryEntry, LogStream};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Tamanho a partir do qual o segmento atual fecha e começa o próximo.
const SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
/// Descarrega o buffer em disco a cada tanto de linhas, ou a cada [`FLUSH_EVERY`].
const FLUSH_LINES: usize = 200;
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// De quanto em quanto tempo o escritor reaplica a retenção por idade em todos os serviços.
const PRUNE_EVERY: Duration = Duration::from_secs(600);
/// Quanto do fim do segmento é lido para descobrir o último instante de um container.
const TAIL_SCAN_BYTES: u64 = 256 * 1024;
const META_FILE: &str = "meta.json";

static GLOBAL: OnceLock<Arc<LogStore>> = OnceLock::new();

/// O armazenamento global, se foi iniciado.
pub fn global() -> Option<Arc<LogStore>> {
    GLOBAL.get().cloned()
}

/// Inicia o armazenamento (uma vez) e o instala como global. `enabled = false` não instala nada.
pub fn init(root: PathBuf, cfg: &LogsConfig) -> Option<Arc<LogStore>> {
    if !cfg.enabled {
        return None;
    }
    let store = Arc::new(LogStore::open(root, cfg.clone()));
    // Já iniciado (segundo `init`): mantém o primeiro.
    let _ = GLOBAL.set(store.clone());
    Some(GLOBAL.get().cloned().unwrap_or(store))
}

#[derive(Serialize, Deserialize)]
struct Record {
    /// Instante (RFC 3339, nanossegundos, UTC).
    t: String,
    /// Id curto do container.
    c: String,
    /// `o` = stdout, `e` = stderr.
    s: String,
    m: String,
}

/// O que se sabe de uma execução (um container) — o `meta.json` da pasta dela.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecMeta {
    /// Id curto do container (também o nome da pasta).
    pub container: String,
    /// Deploy que criou o container (label `rustploy.deployment_id`); `None` em execuções migradas.
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub first_ts: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_ts: Option<DateTime<Utc>>,
    #[serde(default)]
    pub lines: u64,
    #[serde(default)]
    pub bytes: u64,
    /// Segmentos do começo foram apagados pela rotação (a execução sozinha passou do teto).
    #[serde(default)]
    pub partial: bool,
}

enum Msg {
    Line {
        service: String,
        container: String,
        stderr: bool,
        at: DateTime<Utc>,
        text: String,
    },
    /// Deploy e imagem de uma execução (vêm de um `inspect_container`).
    Exec {
        service: String,
        container: String,
        deployment_id: Option<String>,
        image: Option<String>,
    },
    Flush(mpsc::Sender<()>),
    Delete(String),
    /// Apaga as pastas de serviços que não estão na lista.
    Retain(Vec<String>),
    /// Apaga execuções (pastas); responde `(apagadas, bytes liberados)`.
    DeleteExecs {
        service: String,
        containers: Vec<String>,
        ack: mpsc::Sender<(u32, u64)>,
    },
}

pub struct LogStore {
    root: PathBuf,
    tx: Sender<Msg>,
}

/// Id curto (12 caracteres) usado no arquivo e na API.
pub fn short_id(container_id: &str) -> String {
    container_id.chars().take(12).collect()
}

impl LogStore {
    fn open(root: PathBuf, cfg: LogsConfig) -> Self {
        if let Err(e) = create_dir_private(&root) {
            tracing::warn!(error = %e, dir = %root.display(), "log-store: não consegui criar a pasta");
        }
        // Síncrono, no boot: o escritor só começa depois, então nada grava no meio da migração.
        migrate_legacy(&root);
        let (tx, rx) = mpsc::channel();
        let writer_root = root.clone();
        std::thread::Builder::new()
            .name("log-store".into())
            .spawn(move || Writer::new(writer_root, cfg).run(rx))
            .expect("thread do log-store");
        Self { root, tx }
    }

    /// Enfileira uma linha para gravar. Nunca bloqueia.
    pub fn append(
        &self,
        service_id: &str,
        container_id: &str,
        stderr: bool,
        at: DateTime<Utc>,
        text: &str,
    ) {
        let _ = self.tx.send(Msg::Line {
            service: service_id.to_string(),
            container: short_id(container_id),
            stderr,
            at,
            text: text.to_string(),
        });
    }

    /// Registra a que deploy e a que imagem pertence a execução (container). Nunca bloqueia.
    pub fn set_exec(
        &self,
        service_id: &str,
        container_id: &str,
        deployment_id: Option<String>,
        image: Option<String>,
    ) {
        let _ = self.tx.send(Msg::Exec {
            service: service_id.to_string(),
            container: short_id(container_id),
            deployment_id,
            image,
        });
    }

    /// Espera o escritor gravar tudo o que já foi enfileirado. **Bloqueia** (use em `spawn_blocking`).
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(10));
        }
    }

    /// Apaga o histórico de um serviço (serviço removido).
    pub fn delete_service(&self, service_id: &str) {
        let _ = self.tx.send(Msg::Delete(service_id.to_string()));
    }

    /// Apaga as pastas de serviços que não existem mais.
    pub fn retain_services(&self, ids: Vec<String>) {
        let _ = self.tx.send(Msg::Retain(ids));
    }

    /// Último instante gravado para esse container (ou `None`). **Bloqueia** (flush + leitura).
    pub fn last_timestamp(&self, service_id: &str, container_id: &str) -> Option<DateTime<Utc>> {
        self.flush();
        scan_last_ts(&self.root.join(service_id).join(short_id(container_id)))
    }

    /// Até `limit` linhas anteriores a `before` (ou as mais recentes), em ordem cronológica, mais o
    /// instante para pedir a página anterior. `containers` vazio = todas as execuções do serviço.
    /// **Bloqueia** (flush + leitura de disco).
    pub fn read(
        &self,
        service_id: &str,
        before: Option<DateTime<Utc>>,
        limit: usize,
        containers: &[String],
    ) -> (Vec<LogHistoryEntry>, Option<DateTime<Utc>>) {
        self.flush();
        let wanted: Vec<String> = containers.iter().map(|c| short_id(c)).collect();
        read_history(
            &self.root.join(service_id),
            before,
            limit.clamp(1, 5000),
            &wanted,
        )
    }

    /// As execuções guardadas do serviço, da mais nova para a mais velha. **Bloqueia**.
    pub fn executions(&self, service_id: &str) -> Vec<ExecMeta> {
        self.flush();
        let mut out = list_execs(&self.root.join(service_id));
        out.sort_by(|a, b| {
            b.last_ts
                .cmp(&a.last_ts)
                .then(b.container.cmp(&a.container))
        });
        out
    }

    /// Apaga execuções (pastas inteiras). Devolve `(apagadas, bytes liberados)`; id inexistente é
    /// ignorado. Quem chama decide se a execução atual pode sair. **Bloqueia**.
    pub fn delete_executions(&self, service_id: &str, containers: &[String]) -> (u32, u64) {
        let (ack_tx, ack_rx) = mpsc::channel();
        let sent = self.tx.send(Msg::DeleteExecs {
            service: service_id.to_string(),
            containers: containers.iter().map(|c| short_id(c)).collect(),
            ack: ack_tx,
        });
        if sent.is_err() {
            return (0, 0);
        }
        ack_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or((0, 0))
    }
}

// ─── arquivos ────────────────────────────────────────────────────────────────

fn segment_name(idx: u32) -> String {
    format!("{idx:06}.jsonl")
}

/// Segmentos da pasta, `(índice, caminho)` em ordem crescente.
fn segments(dir: &Path) -> Vec<(u32, PathBuf)> {
    let mut out: Vec<(u32, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let idx = name.strip_suffix(".jsonl")?.parse::<u32>().ok()?;
            Some((idx, e.path()))
        })
        .collect();
    out.sort_by_key(|(i, _)| *i);
    out
}

/// Pastas de execução de um serviço (subpastas; os segmentos legados, soltos, não contam).
fn exec_dirs(service_dir: &Path) -> Vec<(String, PathBuf)> {
    fs::read_dir(service_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect()
}

fn dir_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn open_append(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn create_dir_private(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn parse_record(line: &str) -> Option<(DateTime<Utc>, Record)> {
    let rec: Record = serde_json::from_str(line).ok()?;
    let at = DateTime::parse_from_rfc3339(&rec.t)
        .ok()?
        .with_timezone(&Utc);
    Some((at, rec))
}

/// Grava o `meta.json` (escreve num temporário e renomeia, para nunca ficar pela metade).
fn write_meta(dir: &Path, meta: &ExecMeta) {
    let Ok(json) = serde_json::to_vec_pretty(meta) else {
        return;
    };
    let tmp = dir.join(".meta.json.tmp");
    let ok = open_truncate(&tmp)
        .and_then(|mut f| f.write_all(&json))
        .is_ok();
    if ok {
        let _ = fs::rename(&tmp, dir.join(META_FILE));
    }
}

fn open_truncate(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// `meta.json` da pasta; sem ele (ou ilegível), reconstrói varrendo os segmentos.
fn load_meta(dir: &Path, container: &str) -> ExecMeta {
    if let Ok(text) = fs::read_to_string(dir.join(META_FILE))
        && let Ok(mut m) = serde_json::from_str::<ExecMeta>(&text)
    {
        m.container = container.to_string();
        return m;
    }
    derive_meta(dir, container)
}

fn derive_meta(dir: &Path, container: &str) -> ExecMeta {
    let mut m = ExecMeta {
        container: container.to_string(),
        ..Default::default()
    };
    for (_, path) in segments(dir) {
        m.bytes += fs::metadata(&path).map_or(0, |x| x.len());
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (at, _) in text.lines().filter_map(parse_record) {
            m.lines += 1;
            m.first_ts = Some(m.first_ts.map_or(at, |f| f.min(at)));
            m.last_ts = Some(m.last_ts.map_or(at, |l| l.max(at)));
        }
    }
    m
}

fn list_execs(service_dir: &Path) -> Vec<ExecMeta> {
    exec_dirs(service_dir)
        .into_iter()
        .map(|(name, dir)| load_meta(&dir, &name))
        .collect()
}

/// Último instante gravado na pasta de uma execução (só olha o fim do segmento mais novo).
fn scan_last_ts(exec_dir: &Path) -> Option<DateTime<Utc>> {
    let (_, path) = segments(exec_dir).into_iter().next_back()?;
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_SCAN_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter_map(parse_record)
        .map(|(at, _)| at)
        .next()
}

fn entry_of(at: DateTime<Utc>, r: Record) -> LogHistoryEntry {
    LogHistoryEntry {
        timestamp: at,
        container_id: r.c,
        stream: if r.s == "e" {
            LogStream::Stderr
        } else {
            LogStream::Stdout
        },
        line: r.m,
    }
}

/// Até `limit` linhas de **uma** pasta de execução, anteriores a `before`, mais novas primeiro.
/// O `bool` diz se ficou algo mais para trás.
fn read_exec_tail(
    dir: &Path,
    before: Option<DateTime<Utc>>,
    limit: usize,
) -> (Vec<LogHistoryEntry>, bool) {
    let mut collected: Vec<LogHistoryEntry> = Vec::new();
    let mut more = false;
    let segs = segments(dir);
    for (n, (_, path)) in segs.iter().enumerate().rev() {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let mut in_seg: Vec<LogHistoryEntry> = text
            .lines()
            .filter_map(parse_record)
            .filter(|(at, _)| before.is_none_or(|b| *at < b))
            .map(|(at, r)| entry_of(at, r))
            .collect();
        in_seg.reverse();
        collected.extend(in_seg);
        if collected.len() >= limit {
            more = collected.len() > limit || n > 0;
            break;
        }
    }
    collected.truncate(limit);
    (collected, more)
}

fn read_history(
    service_dir: &Path,
    before: Option<DateTime<Utc>>,
    limit: usize,
    containers: &[String],
) -> (Vec<LogHistoryEntry>, Option<DateTime<Utc>>) {
    let mut all: Vec<LogHistoryEntry> = Vec::new();
    let mut more = false;
    for (name, dir) in exec_dirs(service_dir) {
        if !containers.is_empty() && !containers.contains(&name) {
            continue;
        }
        let (got, m) = read_exec_tail(&dir, before, limit);
        more |= m;
        all.extend(got);
    }
    // As mais novas, em ordem cronológica (estável: empate mantém a ordem de leitura).
    all.sort_by_key(|e| e.timestamp);
    if all.len() > limit {
        more = true;
        all.drain(..all.len() - limit);
    }
    let next_before = if more {
        all.first().map(|e| e.timestamp)
    } else {
        None
    };
    (all, next_before)
}

/// Apaga uma pasta de execução; devolve os bytes liberados.
fn remove_exec(dir: &Path) -> u64 {
    let n = dir_bytes(dir);
    if fs::remove_dir_all(dir).is_ok() {
        n
    } else {
        0
    }
}

/// Só segmentos: apaga os mais velhos (menos o mais novo) até a pasta caber em `budget`.
/// Devolve `true` se apagou algum.
fn prune_segments_to(dir: &Path, budget: u64) -> bool {
    let segs = segments(dir);
    let Some((_, rest)) = segs.split_last() else {
        return false;
    };
    let newest = segs
        .last()
        .map_or(0, |(_, p)| fs::metadata(p).map_or(0, |m| m.len()));
    let mut total: u64 = rest
        .iter()
        .map(|(_, p)| fs::metadata(p).map_or(0, |m| m.len()))
        .sum::<u64>()
        + newest;
    let mut pruned = false;
    for (_, path) in rest {
        if total <= budget {
            break;
        }
        let size = fs::metadata(path).map_or(0, |m| m.len());
        if fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
            pruned = true;
        }
    }
    pruned
}

/// Aplica a retenção a um serviço: apaga **execuções inteiras** por idade e por tamanho, da mais velha
/// para a mais nova, poupando a mais recente; se só ela passa do teto, roda a rotação de segmentos
/// dentro dela. Devolve `(execuções apagadas, execuções que viraram parciais)`.
fn prune_service(service_dir: &Path, cfg: &LogsConfig) -> (Vec<String>, Vec<String>) {
    let mut execs: Vec<(ExecMeta, PathBuf, u64)> = exec_dirs(service_dir)
        .into_iter()
        .map(|(name, dir)| {
            let m = load_meta(&dir, &name);
            let size = dir_bytes(&dir);
            (m, dir, size)
        })
        .collect();
    // Mais velha primeiro; a última (mais nova) é a protegida.
    execs.sort_by(|a, b| {
        a.0.last_ts
            .cmp(&b.0.last_ts)
            .then(a.0.container.cmp(&b.0.container))
    });
    let mut removed = Vec::new();
    let mut partial = Vec::new();
    if execs.is_empty() {
        return (removed, partial);
    }
    let protected = execs.len() - 1;
    let max_age = chrono::Duration::days(i64::from(cfg.retention_days));
    let now = Utc::now();
    let mut alive: Vec<(ExecMeta, PathBuf, u64)> = Vec::new();
    for (i, (m, dir, size)) in execs.into_iter().enumerate() {
        let too_old = m.last_ts.is_some_and(|t| now - t > max_age);
        if i != protected && too_old {
            remove_exec(&dir);
            removed.push(m.container.clone());
        } else {
            alive.push((m, dir, size));
        }
    }
    let mut total: u64 = alive.iter().map(|(_, _, n)| *n).sum();
    let mut idx = 0;
    while total > cfg.max_bytes_per_service && idx + 1 < alive.len() {
        let (m, dir, size) = &alive[idx];
        remove_exec(dir);
        removed.push(m.container.clone());
        total = total.saturating_sub(*size);
        idx += 1;
    }
    if total > cfg.max_bytes_per_service
        && let Some((m, dir, size)) = alive.last()
    {
        let budget = cfg
            .max_bytes_per_service
            .saturating_sub(total.saturating_sub(*size));
        if prune_segments_to(dir, budget.max(SEGMENT_BYTES)) {
            partial.push(m.container.clone());
        }
    }
    (removed, partial)
}

// ─── migração dos segmentos da v0.1.40 ───────────────────────────────────────

/// Redistribui por container os segmentos que ficavam soltos na pasta do serviço (layout da v0.1.40),
/// numa subpasta `.mig` que só vira definitiva por `rename`; os segmentos velhos saem por último.
/// Idempotente: interrompida no meio, a próxima carga refaz o que faltou (pasta de container que já
/// existe é considerada pronta e não é tocada).
fn migrate_legacy(root: &Path) {
    let Ok(services) = fs::read_dir(root) else {
        return;
    };
    for svc in services.flatten() {
        let dir = svc.path();
        if !dir.is_dir() {
            continue;
        }
        let legacy = segments(&dir);
        let mig = dir.join(".mig");
        let _ = fs::remove_dir_all(&mig); // sobra de uma migração interrompida
        if legacy.is_empty() {
            continue;
        }
        let result = (|| -> std::io::Result<()> {
            create_dir_private(&mig)?;
            let mut writers: HashMap<String, MigWriter> = HashMap::new();
            for (_, path) in &legacy {
                let text = fs::read_to_string(path)?;
                for (at, rec) in text.lines().filter_map(parse_record) {
                    let c = rec.c.clone();
                    if dir.join(&c).is_dir() {
                        continue; // já migrada numa tentativa anterior
                    }
                    if !writers.contains_key(&c) {
                        let wdir = mig.join(&c);
                        create_dir_private(&wdir)?;
                        writers.insert(c.clone(), MigWriter::new(wdir, &c)?);
                    }
                    if let Some(w) = writers.get_mut(&c) {
                        w.push(at, &rec)?;
                    }
                }
            }
            for (c, w) in writers {
                w.finish()?;
                fs::rename(mig.join(&c), dir.join(&c))?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                for (_, path) in &legacy {
                    let _ = fs::remove_file(path);
                }
                let _ = fs::remove_dir_all(&mig);
                tracing::info!(service = %svc.file_name().to_string_lossy(), "log-store: logs antigos redistribuídos por execução");
            }
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "log-store: migração dos logs antigos falhou; os segmentos antigos ficam e ela se repete no próximo boot");
                let _ = fs::remove_dir_all(&mig);
            }
        }
    }
}

struct MigWriter {
    dir: PathBuf,
    idx: u32,
    out: BufWriter<File>,
    size: u64,
    meta: ExecMeta,
}

impl MigWriter {
    fn new(dir: PathBuf, container: &str) -> std::io::Result<Self> {
        let out = BufWriter::new(open_append(&dir.join(segment_name(1)))?);
        Ok(Self {
            dir,
            idx: 1,
            out,
            size: 0,
            meta: ExecMeta {
                container: container.to_string(),
                ..Default::default()
            },
        })
    }

    fn push(&mut self, at: DateTime<Utc>, rec: &Record) -> std::io::Result<()> {
        let mut json = serde_json::to_vec(rec)?;
        json.push(b'\n');
        if self.size >= SEGMENT_BYTES {
            self.out.flush()?;
            self.idx += 1;
            self.out = BufWriter::new(open_append(&self.dir.join(segment_name(self.idx)))?);
            self.size = 0;
        }
        self.out.write_all(&json)?;
        self.size += json.len() as u64;
        self.meta.bytes += json.len() as u64;
        self.meta.lines += 1;
        self.meta.first_ts = Some(self.meta.first_ts.map_or(at, |f| f.min(at)));
        self.meta.last_ts = Some(self.meta.last_ts.map_or(at, |l| l.max(at)));
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        self.out.flush()?;
        write_meta(&self.dir, &self.meta);
        Ok(())
    }
}

// ─── o escritor ──────────────────────────────────────────────────────────────

struct Open {
    dir: PathBuf,
    idx: u32,
    out: BufWriter<File>,
    size: u64,
    /// Último instante gravado nesta execução.
    last: DateTime<Utc>,
    pending: usize,
    meta: ExecMeta,
    meta_dirty: bool,
}

struct Writer {
    root: PathBuf,
    cfg: LogsConfig,
    /// Execuções abertas, por `(serviço, container curto)`.
    open: HashMap<(String, String), Open>,
    /// Serviços apagados: ids são únicos (ULID), então uma linha atrasada de um stream que ainda
    /// não fechou não pode recriar a pasta.
    deleted: HashSet<String>,
    /// Idem, por execução apagada à mão (`(serviço, container)`).
    deleted_execs: HashSet<(String, String)>,
}

impl Writer {
    fn new(root: PathBuf, cfg: LogsConfig) -> Self {
        Self {
            root,
            cfg,
            open: HashMap::new(),
            deleted: Default::default(),
            deleted_execs: Default::default(),
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        let mut last_flush = Instant::now();
        let mut last_prune = Instant::now();
        loop {
            match rx.recv_timeout(FLUSH_EVERY) {
                Ok(Msg::Line {
                    service,
                    container,
                    stderr,
                    at,
                    text,
                }) => self.line(&service, &container, stderr, at, &text),
                Ok(Msg::Exec {
                    service,
                    container,
                    deployment_id,
                    image,
                }) => self.exec(&service, &container, deployment_id, image),
                Ok(Msg::Flush(ack)) => {
                    self.flush_all();
                    let _ = ack.send(());
                }
                Ok(Msg::Delete(service)) => {
                    self.open.retain(|(s, _), _| s != &service);
                    let _ = fs::remove_dir_all(self.root.join(&service));
                    self.deleted.insert(service);
                }
                Ok(Msg::Retain(keep)) => self.retain(&keep),
                Ok(Msg::DeleteExecs {
                    service,
                    containers,
                    ack,
                }) => {
                    let out = self.delete_execs(&service, &containers);
                    let _ = ack.send(out);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush_all();
                    return;
                }
            }
            if last_flush.elapsed() >= FLUSH_EVERY {
                self.flush_all();
                last_flush = Instant::now();
            }
            if last_prune.elapsed() >= PRUNE_EVERY {
                self.prune_all();
                last_prune = Instant::now();
            }
        }
    }

    fn delete_execs(&mut self, service: &str, containers: &[String]) -> (u32, u64) {
        let mut deleted = 0;
        let mut freed = 0;
        for c in containers {
            let key = (service.to_string(), c.clone());
            self.open.remove(&key);
            let dir = self.root.join(service).join(c);
            if dir.is_dir() {
                freed += remove_exec(&dir);
                deleted += 1;
            }
            self.deleted_execs.insert(key);
        }
        (deleted, freed)
    }

    fn retain(&mut self, keep: &[String]) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && !keep.contains(&name) {
                self.open.retain(|(s, _), _| s != &name);
                let _ = fs::remove_dir_all(e.path());
                self.deleted.insert(name);
            }
        }
    }

    fn prune_all(&mut self) {
        self.flush_all();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for e in entries.flatten() {
            if e.path().is_dir() {
                let name = e.file_name().to_string_lossy().to_string();
                self.prune_one(&name);
            }
        }
    }

    /// Retenção de um serviço, mantendo o estado em memória coerente com o disco.
    fn prune_one(&mut self, service: &str) {
        self.flush_all(); // meta.json em dia: a "mais nova" (protegida) sai dele
        let (removed, partial) = prune_service(&self.root.join(service), &self.cfg);
        for c in removed {
            let key = (service.to_string(), c);
            self.open.remove(&key);
            self.deleted_execs.insert(key);
        }
        for c in partial {
            if let Some(o) = self.open.get_mut(&(service.to_string(), c.clone())) {
                o.meta.partial = true;
                o.meta_dirty = true;
            } else {
                let dir = self.root.join(service).join(&c);
                let mut m = load_meta(&dir, &c);
                m.partial = true;
                write_meta(&dir, &m);
            }
        }
    }

    fn flush_all(&mut self) {
        for o in self.open.values_mut() {
            if o.pending > 0 {
                let _ = o.out.flush();
                o.pending = 0;
            }
            if o.meta_dirty {
                write_meta(&o.dir, &o.meta);
                o.meta_dirty = false;
            }
        }
    }

    /// Abre (ou cria) o segmento mais novo da execução para anexar.
    fn open_exec(&self, service: &str, container: &str) -> std::io::Result<Open> {
        let dir = self.root.join(service).join(container);
        create_dir_private(&self.root.join(service))?;
        create_dir_private(&dir)?;
        let idx = segments(&dir).last().map_or(1, |(i, _)| *i);
        let path = dir.join(segment_name(idx));
        // Queda no meio de uma linha: fecha a linha parcial para a próxima começar limpa.
        let needs_newline = fs::read(&path)
            .ok()
            .is_some_and(|b| b.last().is_some_and(|c| *c != b'\n'));
        let file = open_append(&path)?;
        let size = file.metadata()?.len();
        let mut out = BufWriter::new(file);
        if needs_newline {
            out.write_all(b"\n")?;
        }
        let had_meta = dir.join(META_FILE).is_file();
        let mut meta = load_meta(&dir, container);
        let on_disk = scan_last_ts(&dir);
        // O instante do disco vence um `meta.json` atrasado (queda antes do flush do meta).
        meta.last_ts = meta.last_ts.max(on_disk);
        Ok(Open {
            dir,
            idx,
            out,
            size,
            last: meta.last_ts.unwrap_or(DateTime::<Utc>::MIN_UTC),
            pending: 0,
            meta,
            meta_dirty: !had_meta,
        })
    }

    fn get_open(&mut self, service: &str, container: &str) -> Option<&mut Open> {
        let key = (service.to_string(), container.to_string());
        if self.deleted.contains(service) || self.deleted_execs.contains(&key) {
            return None;
        }
        if !self.open.contains_key(&key) {
            match self.open_exec(service, container) {
                Ok(o) => {
                    self.open.insert(key.clone(), o);
                }
                Err(e) => {
                    tracing::warn!(error = %e, service, container, "log-store: não consegui abrir o segmento");
                    return None;
                }
            }
        }
        self.open.get_mut(&key)
    }

    fn exec(
        &mut self,
        service: &str,
        container: &str,
        deployment_id: Option<String>,
        image: Option<String>,
    ) {
        let Some(o) = self.get_open(service, container) else {
            return;
        };
        if deployment_id.is_some() && o.meta.deployment_id != deployment_id {
            o.meta.deployment_id = deployment_id;
            o.meta_dirty = true;
        }
        if image.is_some() && o.meta.image != image {
            o.meta.image = image;
            o.meta_dirty = true;
        }
    }

    fn line(
        &mut self,
        service: &str,
        container: &str,
        stderr: bool,
        at: DateTime<Utc>,
        text: &str,
    ) {
        let Some(o) = self.get_open(service, container) else {
            return;
        };
        // Repetida (reconexão do stream / captura final)? O último instante vem do disco na 1ª vez.
        if at <= o.last {
            return;
        }
        o.last = at;

        let rec = Record {
            t: at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            c: container.to_string(),
            s: if stderr { "e" } else { "o" }.to_string(),
            m: text.to_string(),
        };
        let Ok(mut json) = serde_json::to_vec(&rec) else {
            return;
        };
        json.push(b'\n');
        if o.out.write_all(&json).is_err() {
            return;
        }
        o.size += json.len() as u64;
        o.pending += 1;
        o.meta.lines += 1;
        o.meta.bytes += json.len() as u64;
        o.meta.first_ts.get_or_insert(at);
        o.meta.last_ts = Some(at);
        o.meta_dirty = true;
        if o.pending >= FLUSH_LINES {
            let _ = o.out.flush();
            o.pending = 0;
        }
        if o.size >= SEGMENT_BYTES {
            let _ = o.out.flush();
            let next = o.idx + 1;
            match open_append(&o.dir.join(segment_name(next))) {
                Ok(file) => {
                    o.out = BufWriter::new(file);
                    o.idx = next;
                    o.size = 0;
                    o.pending = 0;
                    // a retenção do serviço (execuções velhas, ou segmentos desta) roda a cada rotação
                    self.prune_one(service);
                }
                Err(e) => {
                    tracing::warn!(error = %e, service, "log-store: não consegui rotacionar o segmento")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cfg() -> LogsConfig {
        LogsConfig {
            enabled: true,
            retention_days: 14,
            max_bytes_per_service: 50 * 1024 * 1024,
        }
    }

    fn ts(sec: i64, nanos: u32) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + sec, nanos).unwrap()
    }

    fn store() -> (LogStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        (LogStore::open(tmp.path().join("logs"), cfg()), tmp)
    }

    fn lines(v: &[LogHistoryEntry]) -> Vec<&str> {
        v.iter().map(|e| e.line.as_str()).collect()
    }

    #[test]
    fn grava_e_le_em_ordem_cronologica() {
        let (s, _t) = store();
        for i in 0..5 {
            s.append(
                "svc_a",
                "aaaaaaaaaaaaaaaa",
                i % 2 == 1,
                ts(i, 0),
                &format!("linha {i}"),
            );
        }
        let (got, next) = s.read("svc_a", None, 100, &[]);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].line, "linha 0");
        assert_eq!(got[4].line, "linha 4");
        assert_eq!(got[1].stream, LogStream::Stderr);
        assert_eq!(
            got[0].container_id, "aaaaaaaaaaaa",
            "id curto de 12 caracteres"
        );
        assert_eq!(next, None, "nada mais para trás");
    }

    #[test]
    fn pagina_para_tras_com_next_before() {
        let (s, _t) = store();
        for i in 0..10 {
            s.append("svc_a", "c1", false, ts(i, 0), &format!("l{i}"));
        }
        let (p1, next1) = s.read("svc_a", None, 4, &[]);
        assert_eq!(lines(&p1), ["l6", "l7", "l8", "l9"]);
        let (p2, next2) = s.read("svc_a", next1, 4, &[]);
        assert_eq!(lines(&p2), ["l2", "l3", "l4", "l5"]);
        let (p3, next3) = s.read("svc_a", next2, 4, &[]);
        assert_eq!(lines(&p3), ["l0", "l1"]);
        assert_eq!(next3, None);
    }

    #[test]
    fn nao_duplica_linha_repetida_nem_antiga() {
        let (s, _t) = store();
        s.append("svc_a", "c1", false, ts(1, 0), "a");
        s.append("svc_a", "c1", false, ts(2, 0), "b");
        // reconexão do stream reenvia as últimas linhas
        s.append("svc_a", "c1", false, ts(1, 0), "a");
        s.append("svc_a", "c1", false, ts(2, 0), "b");
        s.append("svc_a", "c1", false, ts(3, 0), "c");
        let (got, _) = s.read("svc_a", None, 100, &[]);
        assert_eq!(lines(&got), ["a", "b", "c"]);
    }

    #[test]
    fn dedup_sobrevive_a_reinicio_do_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        {
            let s = LogStore::open(root.clone(), cfg());
            s.append("svc_a", "c1", false, ts(1, 0), "a");
            s.append("svc_a", "c1", false, ts(2, 0), "b");
            s.flush();
        }
        let s = LogStore::open(root, cfg());
        s.append("svc_a", "c1", false, ts(2, 0), "b"); // o stream reabriu com tail
        s.append("svc_a", "c1", false, ts(3, 0), "c");
        let (got, _) = s.read("svc_a", None, 100, &[]);
        assert_eq!(lines(&got), ["a", "b", "c"]);
        assert_eq!(s.last_timestamp("svc_a", "c1"), Some(ts(3, 0)));
    }

    #[test]
    fn historico_atravessa_a_troca_de_container_e_filtra() {
        let (s, _t) = store();
        s.append("svc_a", "velho_container", false, ts(1, 0), "do velho");
        s.append("svc_a", "novo_container", true, ts(5, 0), "do novo");
        let (todos, _) = s.read("svc_a", None, 100, &[]);
        assert_eq!(
            lines(&todos),
            ["do velho", "do novo"],
            "intercalado por instante"
        );
        assert_ne!(todos[0].container_id, todos[1].container_id);
        let (so_velho, _) = s.read("svc_a", None, 100, &["velho_container".to_string()]);
        assert_eq!(lines(&so_velho), ["do velho"]);
        let (dois, _) = s.read(
            "svc_a",
            None,
            100,
            &["velho_container".to_string(), "novo_container".to_string()],
        );
        assert_eq!(dois.len(), 2);
    }

    #[test]
    fn linha_parcial_no_fim_do_arquivo_e_ignorada_e_fechada() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let dir = root.join("svc_a").join("c1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("000001.jsonl"),
            "{\"t\":\"2027-01-15T08:00:01.000000000Z\",\"c\":\"c1\",\"s\":\"o\",\"m\":\"ok\"}\n{\"t\":\"2027-01-15T08:00:02.0000",
        )
        .unwrap();
        let s = LogStore::open(root, cfg());
        s.append(
            "svc_a",
            "c1",
            false,
            Utc.with_ymd_and_hms(2027, 1, 15, 8, 0, 9).unwrap(),
            "depois",
        );
        let (got, _) = s.read("svc_a", None, 100, &[]);
        assert_eq!(lines(&got), ["ok", "depois"]);
    }

    #[test]
    fn rotaciona_segmentos_e_respeita_o_tamanho_maximo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let mut c = cfg();
        c.max_bytes_per_service = 9 * 1024 * 1024; // ~2 segmentos de 4 MiB + o atual
        let s = LogStore::open(root.clone(), c);
        let linha = "x".repeat(1000);
        for i in 0..20_000 {
            s.append("svc_a", "c1", false, ts(0, i + 1), &linha);
        }
        s.flush();
        let dir = root.join("svc_a").join("c1");
        let segs = segments(&dir);
        assert!(segs.len() >= 2, "rotacionou: {}", segs.len());
        let total: u64 = segs
            .iter()
            .map(|(_, p)| fs::metadata(p).unwrap().len())
            .sum();
        assert!(
            total <= 9 * 1024 * 1024 + SEGMENT_BYTES,
            "dentro do teto (mais o segmento atual): {total}"
        );
        assert!(
            segs.first().unwrap().0 > 1,
            "os segmentos mais velhos foram apagados"
        );
        let metas = s.executions("svc_a");
        assert_eq!(metas.len(), 1);
        assert!(
            metas[0].partial,
            "a execução sozinha passou do teto: parcial"
        );
    }

    #[test]
    fn apagar_servico_remove_a_pasta_e_retain_limpa_orfas() {
        let (s, t) = store();
        s.append("svc_a", "c1", false, ts(1, 0), "a");
        s.append("svc_b", "c1", false, ts(1, 0), "b");
        s.flush();
        s.delete_service("svc_a");
        s.flush();
        assert!(!t.path().join("logs/svc_a").exists());
        s.retain_services(vec![]);
        s.flush();
        assert!(!t.path().join("logs/svc_b").exists());
        // linha atrasada de um stream que ainda não fechou não recria a pasta
        s.append("svc_a", "c1", false, ts(9, 0), "tardia");
        s.flush();
        assert!(!t.path().join("logs/svc_a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn pasta_e_arquivos_sao_privados() {
        use std::os::unix::fs::PermissionsExt;
        let (s, t) = store();
        s.append("svc_a", "c1", false, ts(1, 0), "a");
        s.flush();
        let svc = t.path().join("logs/svc_a");
        let dir = svc.join("c1");
        for d in [&svc, &dir] {
            assert_eq!(fs::metadata(d).unwrap().permissions().mode() & 0o777, 0o700);
        }
        for f in ["000001.jsonl", "meta.json"] {
            assert_eq!(
                fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777,
                0o600,
                "{f}"
            );
        }
    }

    #[test]
    fn lista_execucoes_com_meta_e_deploy() {
        let (s, _t) = store();
        s.append("svc_a", "velho", false, ts(1, 0), "a");
        s.append("svc_a", "velho", true, ts(2, 0), "b");
        s.append("svc_a", "novo", false, ts(10, 0), "c");
        s.set_exec("svc_a", "velho", Some("dep_1".into()), Some("img:1".into()));
        s.set_exec("svc_a", "novo", Some("dep_2".into()), None);
        let execs = s.executions("svc_a");
        assert_eq!(execs.len(), 2);
        // da mais nova para a mais velha
        assert_eq!(execs[0].container, "novo");
        assert_eq!(execs[0].deployment_id.as_deref(), Some("dep_2"));
        assert_eq!(execs[0].lines, 1);
        assert_eq!(execs[1].container, "velho");
        assert_eq!(execs[1].image.as_deref(), Some("img:1"));
        assert_eq!(execs[1].lines, 2);
        assert_eq!(execs[1].first_ts, Some(ts(1, 0)));
        assert_eq!(execs[1].last_ts, Some(ts(2, 0)));
        assert!(execs[1].bytes > 0);
        assert!(!execs[1].partial);
    }

    #[test]
    fn meta_sobrevive_a_reinicio_e_continua_a_contar() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        {
            let s = LogStore::open(root.clone(), cfg());
            s.append("svc_a", "c1", false, ts(1, 0), "a");
            s.set_exec("svc_a", "c1", Some("dep_9".into()), None);
            s.flush();
        }
        let s = LogStore::open(root, cfg());
        s.append("svc_a", "c1", false, ts(2, 0), "b");
        let e = &s.executions("svc_a")[0];
        assert_eq!(e.lines, 2);
        assert_eq!(e.deployment_id.as_deref(), Some("dep_9"));
        assert_eq!(e.first_ts, Some(ts(1, 0)));
        assert_eq!(e.last_ts, Some(ts(2, 0)));
    }

    #[test]
    fn exclui_execucoes_em_lote_e_ignora_inexistente() {
        let (s, t) = store();
        s.append("svc_a", "c1", false, ts(1, 0), "a");
        s.append("svc_a", "c2", false, ts(2, 0), "b");
        s.append("svc_a", "c3", false, ts(3, 0), "c");
        s.flush();
        let (n, freed) =
            s.delete_executions("svc_a", &["c1".into(), "c2".into(), "naoexiste".into()]);
        assert_eq!(n, 2);
        assert!(freed > 0);
        assert!(!t.path().join("logs/svc_a/c1").exists());
        assert!(!t.path().join("logs/svc_a/c2").exists());
        let (got, _) = s.read("svc_a", None, 100, &[]);
        assert_eq!(lines(&got), ["c"]);
        // linha atrasada de um stream da execução apagada não a recria
        s.append("svc_a", "c1", false, ts(9, 0), "tardia");
        s.flush();
        assert!(!t.path().join("logs/svc_a/c1").exists());
    }

    #[test]
    fn retencao_por_idade_apaga_execucoes_inteiras_e_poupa_a_mais_nova() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let svc = root.join("svc_a");
        let mk = |name: &str, days_ago: i64| {
            let d = svc.join(name);
            fs::create_dir_all(&d).unwrap();
            let t = Utc::now() - chrono::Duration::days(days_ago);
            let rec = format!(
                "{{\"t\":\"{}\",\"c\":\"{name}\",\"s\":\"o\",\"m\":\"x\"}}\n",
                t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            );
            fs::write(d.join("000001.jsonl"), rec).unwrap();
        };
        mk("velha", 30);
        mk("media", 20);
        mk("nova", 19); // todas passam de 14 dias, mas a mais recente é a protegida
        let (removed, _) = prune_service(&svc, &cfg());
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"velha".to_string()));
        assert!(removed.contains(&"media".to_string()));
        assert!(!svc.join("velha").exists());
        assert!(!svc.join("media").exists());
        assert!(svc.join("nova").exists(), "a mais nova nunca sai por idade");
    }

    #[test]
    fn retencao_por_tamanho_apaga_a_execucao_mais_velha_primeiro() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let s = LogStore::open(root.clone(), cfg());
        let big = "y".repeat(1000);
        for (i, cont) in ["c1", "c2", "c3"].iter().enumerate() {
            for j in 0..2 {
                s.append("svc_a", cont, false, ts(i as i64 * 10 + j, 0), &big);
            }
        }
        s.flush();
        // força a retenção (o escritor só roda a cada 10 min ou a cada rotação)
        // 3 execuções de ~2 KB cada; teto de 3000 B: saem as duas mais velhas, a mais nova fica
        let (removed, _) = prune_service(
            &root.join("svc_a"),
            &LogsConfig {
                max_bytes_per_service: 3000,
                ..cfg()
            },
        );
        assert_eq!(removed, ["c1".to_string(), "c2".to_string()]);
        assert!(root.join("svc_a/c3").exists());
    }

    #[test]
    fn migra_segmentos_antigos_para_pastas_por_container() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let svc = root.join("svc_a");
        fs::create_dir_all(&svc).unwrap();
        let rec = |t: &str, c: &str, m: &str| {
            format!("{{\"t\":\"{t}\",\"c\":\"{c}\",\"s\":\"o\",\"m\":\"{m}\"}}\n")
        };
        let mut seg1 = String::new();
        seg1 += &rec("2027-01-15T08:00:01.000000000Z", "velho", "v1");
        seg1 += &rec("2027-01-15T08:00:02.000000000Z", "velho", "v2");
        let mut seg2 = String::new();
        seg2 += &rec("2027-01-15T09:00:00.000000000Z", "novo", "n1");
        fs::write(svc.join("000001.jsonl"), seg1).unwrap();
        fs::write(svc.join("000002.jsonl"), seg2).unwrap();

        let s = LogStore::open(root.clone(), cfg());
        assert!(!svc.join("000001.jsonl").exists(), "segmentos antigos saem");
        assert!(!svc.join(".mig").exists());
        let execs = s.executions("svc_a");
        assert_eq!(execs.len(), 2);
        assert_eq!(execs[0].container, "novo");
        assert_eq!(execs[0].deployment_id, None, "sem deploy associado");
        assert_eq!(execs[1].lines, 2);
        let (got, _) = s.read("svc_a", None, 100, &[]);
        assert_eq!(lines(&got), ["v1", "v2", "n1"]);
        // idempotente: abrir de novo não muda nada
        drop(s);
        let s2 = LogStore::open(root, cfg());
        assert_eq!(s2.executions("svc_a").len(), 2);
    }

    #[test]
    fn migracao_interrompida_e_refeita() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let svc = root.join("svc_a");
        fs::create_dir_all(svc.join(".mig/velho")).unwrap(); // sobra de uma tentativa interrompida
        fs::write(svc.join(".mig/velho/lixo.txt"), "x").unwrap();
        fs::write(
            svc.join("000001.jsonl"),
            "{\"t\":\"2027-01-15T08:00:01.000000000Z\",\"c\":\"velho\",\"s\":\"o\",\"m\":\"v1\"}\n",
        )
        .unwrap();
        let s = LogStore::open(root, cfg());
        let (got, _) = s.read("svc_a", None, 100, &[]);
        assert_eq!(lines(&got), ["v1"]);
        assert!(!svc.join(".mig").exists());
    }
}
