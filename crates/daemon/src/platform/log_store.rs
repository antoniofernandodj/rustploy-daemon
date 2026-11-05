//! Armazenamento do **log de runtime** dos containers (plano: `docs/plano-log-de-runtime-persistido.md`).
//!
//! Por serviço, uma pasta `<dados>/logs/<service_id>/` com *segmentos* `000001.jsonl`, `000002.jsonl`…
//! Cada linha do arquivo é um JSON `{"t","c","s","m"}` (instante, container curto, `o`/`e`, texto). Um
//! segmento fecha ao passar de [`SEGMENT_BYTES`]; a retenção apaga segmentos inteiros, do mais velho
//! para o mais novo, por idade (`retention_days`) e por tamanho (`max_bytes_per_service`).
//!
//! Um **único escritor** (thread própria) recebe as linhas por um canal e grava em lote, para o caminho
//! quente do daemon nunca esperar o disco. É ele também quem descarta linhas repetidas: guarda o último
//! instante gravado por container, então reconectar um stream (ou capturar o que sobrou antes de
//! destruir o container) não duplica nada.
//!
//! O armazenamento é processo-global ([`init`]/[`global`]): o deploy, o streaming e os handlers precisam
//! dele e passá-lo por todos os construtores não compensava. Sem [`init`] (testes, `enabled = false`),
//! [`global`] devolve `None` e tudo vira no-op.

use crate::platform::config::LogsConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::protocol::{LogHistoryEntry, LogStream};
use std::collections::HashMap;
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

enum Msg {
    Line {
        service: String,
        container: String,
        stderr: bool,
        at: DateTime<Utc>,
        text: String,
    },
    Flush(mpsc::Sender<()>),
    Delete(String),
    /// Apaga as pastas de serviços que não estão na lista.
    Retain(Vec<String>),
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
        scan_last_ts(&self.root.join(service_id), &short_id(container_id))
    }

    /// Até `limit` linhas anteriores a `before` (ou as mais recentes), em ordem cronológica, mais o
    /// instante para pedir a página anterior. **Bloqueia** (flush + leitura de disco).
    pub fn read(
        &self,
        service_id: &str,
        before: Option<DateTime<Utc>>,
        limit: usize,
        container: Option<&str>,
    ) -> (Vec<LogHistoryEntry>, Option<DateTime<Utc>>) {
        self.flush();
        read_history(
            &self.root.join(service_id),
            before,
            limit.clamp(1, 5000),
            container.map(short_id).as_deref(),
        )
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

/// Último instante de `container` no segmento mais novo (só olha o fim do arquivo).
fn scan_last_ts(dir: &Path, container: &str) -> Option<DateTime<Utc>> {
    let (_, path) = segments(dir).into_iter().next_back()?;
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
        .find(|(_, r)| r.c == container)
        .map(|(at, _)| at)
}

fn read_history(
    dir: &Path,
    before: Option<DateTime<Utc>>,
    limit: usize,
    container: Option<&str>,
) -> (Vec<LogHistoryEntry>, Option<DateTime<Utc>>) {
    // Do segmento mais novo para o mais velho, até juntar `limit` linhas.
    let mut collected: Vec<LogHistoryEntry> = Vec::new(); // do mais novo para o mais velho
    let mut more = false;
    let segs = segments(dir);
    for (n, (_, path)) in segs.iter().enumerate().rev() {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let mut in_seg: Vec<LogHistoryEntry> = text
            .lines()
            .filter_map(parse_record)
            .filter(|(at, r)| before.is_none_or(|b| *at < b) && container.is_none_or(|c| r.c == c))
            .map(|(at, r)| LogHistoryEntry {
                timestamp: at,
                container_id: r.c,
                stream: if r.s == "e" {
                    LogStream::Stderr
                } else {
                    LogStream::Stdout
                },
                line: r.m,
            })
            .collect();
        in_seg.reverse();
        collected.extend(in_seg);
        if collected.len() >= limit {
            more = collected.len() > limit || n > 0;
            break;
        }
    }
    collected.truncate(limit);
    collected.reverse();
    let next_before = if more {
        collected.first().map(|e| e.timestamp)
    } else {
        None
    };
    (collected, next_before)
}

/// Aplica a retenção: por idade (segmentos antigos) e por tamanho. O segmento mais novo nunca sai.
fn prune_dir(dir: &Path, cfg: &LogsConfig) {
    let segs = segments(dir);
    let Some((_, rest)) = segs.split_last() else {
        return;
    };
    let max_age = Duration::from_secs(u64::from(cfg.retention_days) * 86_400);
    let now = std::time::SystemTime::now();
    let mut alive: Vec<(PathBuf, u64)> = Vec::new();
    for (_, path) in rest {
        let meta = fs::metadata(path).ok();
        let too_old = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if too_old {
            let _ = fs::remove_file(path);
        } else {
            alive.push((path.clone(), meta.map_or(0, |m| m.len())));
        }
    }
    let newest = segs
        .last()
        .map_or(0, |(_, p)| fs::metadata(p).map_or(0, |m| m.len()));
    let mut total: u64 = alive.iter().map(|(_, n)| *n).sum::<u64>() + newest;
    for (path, size) in alive {
        if total <= cfg.max_bytes_per_service {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

// ─── o escritor ──────────────────────────────────────────────────────────────

struct Open {
    dir: PathBuf,
    idx: u32,
    out: BufWriter<File>,
    size: u64,
    /// Último instante gravado, por container curto.
    last: HashMap<String, DateTime<Utc>>,
    pending: usize,
}

struct Writer {
    root: PathBuf,
    cfg: LogsConfig,
    open: HashMap<String, Open>,
    /// Serviços apagados: ids são únicos (ULID), então uma linha atrasada de um stream que ainda
    /// não fechou não pode recriar a pasta.
    deleted: std::collections::HashSet<String>,
}

impl Writer {
    fn new(root: PathBuf, cfg: LogsConfig) -> Self {
        if let Err(e) = create_dir_private(&root) {
            tracing::warn!(error = %e, dir = %root.display(), "log-store: não consegui criar a pasta");
        }
        Self {
            root,
            cfg,
            open: HashMap::new(),
            deleted: Default::default(),
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
                Ok(Msg::Flush(ack)) => {
                    self.flush_all();
                    let _ = ack.send(());
                }
                Ok(Msg::Delete(service)) => {
                    self.open.remove(&service);
                    let _ = fs::remove_dir_all(self.root.join(&service));
                    self.deleted.insert(service);
                }
                Ok(Msg::Retain(keep)) => self.retain(&keep),
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

    fn retain(&mut self, keep: &[String]) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && !keep.contains(&name) {
                self.open.remove(&name);
                let _ = fs::remove_dir_all(e.path());
                self.deleted.insert(name);
            }
        }
    }

    fn prune_all(&mut self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for e in entries.flatten() {
            if e.path().is_dir() {
                prune_dir(&e.path(), &self.cfg);
            }
        }
    }

    fn flush_all(&mut self) {
        for o in self.open.values_mut() {
            if o.pending > 0 {
                let _ = o.out.flush();
                o.pending = 0;
            }
        }
    }

    /// Abre (ou cria) o segmento mais novo do serviço para anexar.
    fn open_service(&self, service: &str) -> std::io::Result<Open> {
        let dir = self.root.join(service);
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
        Ok(Open {
            dir,
            idx,
            out,
            size,
            last: HashMap::new(),
            pending: 0,
        })
    }

    fn line(
        &mut self,
        service: &str,
        container: &str,
        stderr: bool,
        at: DateTime<Utc>,
        text: &str,
    ) {
        if self.deleted.contains(service) {
            return;
        }
        if !self.open.contains_key(service) {
            match self.open_service(service) {
                Ok(o) => {
                    self.open.insert(service.to_string(), o);
                }
                Err(e) => {
                    tracing::warn!(error = %e, service, "log-store: não consegui abrir o segmento");
                    return;
                }
            }
        }
        let cfg = self.cfg.clone();
        let Some(o) = self.open.get_mut(service) else {
            return;
        };
        // Repetida (reconexão do stream / captura final)? O último instante vem do disco na 1ª vez.
        let last = *o
            .last
            .entry(container.to_string())
            .or_insert_with(|| scan_last_ts(&o.dir, container).unwrap_or(DateTime::<Utc>::MIN_UTC));
        if at <= last {
            return;
        }
        o.last.insert(container.to_string(), at);

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
                    prune_dir(&o.dir, &cfg);
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
        let (got, next) = s.read("svc_a", None, 100, None);
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
        let (p1, next1) = s.read("svc_a", None, 4, None);
        assert_eq!(
            p1.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            ["l6", "l7", "l8", "l9"]
        );
        let (p2, next2) = s.read("svc_a", next1, 4, None);
        assert_eq!(
            p2.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            ["l2", "l3", "l4", "l5"]
        );
        let (p3, next3) = s.read("svc_a", next2, 4, None);
        assert_eq!(
            p3.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            ["l0", "l1"]
        );
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
        let (got, _) = s.read("svc_a", None, 100, None);
        assert_eq!(
            got.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
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
        let (got, _) = s.read("svc_a", None, 100, None);
        assert_eq!(
            got.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(s.last_timestamp("svc_a", "c1"), Some(ts(3, 0)));
    }

    #[test]
    fn historico_atravessa_a_troca_de_container_e_filtra() {
        let (s, _t) = store();
        s.append("svc_a", "velho_container", false, ts(1, 0), "do velho");
        s.append("svc_a", "novo_container", true, ts(5, 0), "do novo");
        let (todos, _) = s.read("svc_a", None, 100, None);
        assert_eq!(todos.len(), 2);
        assert_ne!(todos[0].container_id, todos[1].container_id);
        let (so_velho, _) = s.read("svc_a", None, 100, Some("velho_container"));
        assert_eq!(so_velho.len(), 1);
        assert_eq!(so_velho[0].line, "do velho");
    }

    #[test]
    fn linha_parcial_no_fim_do_arquivo_e_ignorada_e_fechada() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let dir = root.join("svc_a");
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
        let (got, _) = s.read("svc_a", None, 100, None);
        assert_eq!(
            got.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            ["ok", "depois"]
        );
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
        let segs = segments(&root.join("svc_a"));
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
        let dir = t.path().join("logs/svc_a");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let seg = dir.join("000001.jsonl");
        assert_eq!(
            fs::metadata(seg).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
