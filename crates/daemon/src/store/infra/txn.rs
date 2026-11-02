//! Transação de "tudo ou nada" sobre um conjunto de arquivos num diretório.
//!
//! # Como funciona
//! Uma escrita (`FileTree::commit`) segue estes passos:
//! 1. grava os arquivos novos em `.staging/<txid>/files/…` (com `fsync` em cada um);
//! 2. grava `PLAN.json` (o que será escrito e o que será apagado);
//! 3. grava o marcador `COMMIT` — **o ponto de não retorno**;
//! 4. move (`rename`, atômico por arquivo) cada arquivo do staging para o lugar final e apaga os
//!    removidos;
//! 5. apaga o staging.
//!
//! `rename` de **um** arquivo é atômico; de vários, não. A atomicidade do conjunto vem do
//! marcador: se o processo cair **antes** do `COMMIT`, a próxima abertura descarta o staging
//! (estado antigo, inteiro); se cair **depois**, a próxima abertura **termina de aplicar** o
//! staging (estado novo, inteiro). Nunca fica misturado.
//!
//! # Concorrência
//! - Entre processos: `flock` exclusivo em `.lock` (segunda instância recusa subir).
//! - Dentro do processo: um `Mutex` serializa `commit`.
//! - **Leituras do daemon devem vir da cópia em memória**, não deste módulo (ver plano R3). `read`
//!   e `list` existem para carregar o estado na subida e para testes; não são atômicos contra um
//!   `commit` concorrente.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, TryLockError},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tracing::{info, warn};

const STAGING: &str = ".staging";
const LOCK: &str = ".lock";
const COMMIT: &str = "COMMIT";
const PLAN: &str = "PLAN.json";
const FILES: &str = "files";

/// Conjunto de mudanças aplicado atomicamente por [`FileTree::commit`].
#[derive(Debug, Default, Clone)]
pub struct Tx {
    writes: BTreeMap<String, Vec<u8>>,
    deletes: BTreeSet<String>,
}

impl Tx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cria ou sobrescreve `rel` (caminho relativo, com `/`).
    pub fn write(&mut self, rel: impl Into<String>, bytes: impl Into<Vec<u8>>) -> &mut Self {
        let rel = rel.into();
        self.deletes.remove(&rel);
        self.writes.insert(rel, bytes.into());
        self
    }

    /// Apaga `rel` (sem erro se não existir).
    pub fn delete(&mut self, rel: impl Into<String>) -> &mut Self {
        let rel = rel.into();
        self.writes.remove(&rel);
        self.deletes.insert(rel);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.deletes.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
struct Plan {
    writes: Vec<String>,
    deletes: Vec<String>,
}

/// Diretório de arquivos com escrita transacional.
pub struct FileTree {
    root: PathBuf,
    /// Mantém o `flock` enquanto o `FileTree` existir.
    _lock: File,
    commit_gate: Mutex<()>,
    #[cfg(test)]
    crash_at: Mutex<Option<Step>>,
}

/// Pontos em que os testes simulam uma queda do processo (sem nenhuma limpeza).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Arquivos gravados no staging, sem `PLAN.json` ainda.
    AfterStagingFiles,
    /// `PLAN.json` gravado, sem `COMMIT`.
    AfterPlan,
    /// `COMMIT` gravado, nada aplicado.
    AfterCommitMarker,
    /// Depois de aplicar `n` operações (renames/remoções) do plano.
    AfterApply(usize),
    /// Tudo aplicado, staging ainda não apagado.
    BeforeCleanup,
}

#[cfg(test)]
#[derive(Debug)]
struct InjectedCrash(Step);
#[cfg(test)]
impl std::fmt::Display for InjectedCrash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "queda injetada em {:?}", self.0)
    }
}
#[cfg(test)]
impl std::error::Error for InjectedCrash {}

impl FileTree {
    /// Abre (criando se preciso) o diretório, toma o lock e **recupera** qualquer transação
    /// interrompida por uma queda anterior.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("criando {}", root.display()))?;
        // O diretório inteiro é só do dono (0700): até a Fase 5 o YAML guarda senhas de banco em
        // claro, e depois dela ainda guarda texto cifrado que não precisa ser lido por outros.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("restringindo {}", root.display()))?;
        }
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(LOCK))
            .context("abrindo o arquivo de lock")?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!(
                "o diretório de infra {} já está em uso por outra instância do rustploy",
                root.display()
            ),
            Err(TryLockError::Error(e)) => return Err(e).context("tomando o lock"),
        }
        let tree = Self {
            root,
            _lock: lock,
            commit_gate: Mutex::new(()),
            #[cfg(test)]
            crash_at: Mutex::new(None),
        };
        tree.recover()?;
        Ok(tree)
    }

    // Ainda sem uso em produção: a Fase 6 (git) precisa do caminho para rodar `git -C <root>`.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lê um arquivo; `None` se não existir.
    pub fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        check_rel(rel)?;
        match fs::read(self.root.join(rel)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("lendo {rel}")),
        }
    }

    /// Todos os arquivos (caminhos relativos, ordenados), ignorando tudo que começa com `.`
    /// (`.lock`, `.staging`, `.git`…).
    pub fn list(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        walk(&self.root, "", &mut out)?;
        out.sort();
        Ok(out)
    }

    /// Aplica a transação inteira ou nada. Veja o cabeçalho do módulo.
    pub fn commit(&self, tx: Tx) -> Result<()> {
        if tx.is_empty() {
            return Ok(());
        }
        for rel in tx.writes.keys().chain(tx.deletes.iter()) {
            check_rel(rel)?;
        }
        let _gate = self
            .commit_gate
            .lock()
            .map_err(|_| anyhow!("lock envenenado"))?;

        let txid = ulid::Ulid::new().to_string();
        let txdir = self.root.join(STAGING).join(&txid);

        match self.stage_and_apply(&tx, &txdir) {
            Ok(()) => Ok(()),
            Err(e) => {
                #[cfg(test)]
                if e.downcast_ref::<InjectedCrash>().is_some() {
                    return Err(e); // "queda": não limpa nada, como um processo que morreu
                }
                if txdir.join(COMMIT).exists() {
                    // Já passou do ponto de não retorno: não dá para voltar, só terminar.
                    warn!(error = %e, "commit falhou depois do COMMIT; tentando terminar a aplicação");
                    self.recover()
                        .context("terminando a transação após falha")?;
                    return Ok(());
                }
                let _ = fs::remove_dir_all(&txdir);
                Err(e)
            }
        }
    }

    fn stage_and_apply(&self, tx: &Tx, txdir: &Path) -> Result<()> {
        let files = txdir.join(FILES);
        fs::create_dir_all(&files)?;

        // 1. Arquivos no staging.
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for (rel, bytes) in &tx.writes {
            let dst = files.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
                dirs.insert(parent.to_path_buf());
            }
            write_synced(&dst, bytes)?;
        }
        for d in &dirs {
            sync_dir(d)?;
        }
        self.crash(Step_::AfterStagingFiles)?;

        // 2. Plano.
        let plan = Plan {
            writes: tx.writes.keys().cloned().collect(),
            deletes: tx.deletes.iter().cloned().collect(),
        };
        write_synced(&txdir.join(PLAN), &serde_json::to_vec(&plan)?)?;
        sync_dir(txdir)?;
        self.crash(Step_::AfterPlan)?;

        // 3. Ponto de não retorno.
        write_synced(&txdir.join(COMMIT), b"ok")?;
        sync_dir(txdir)?;
        self.crash(Step_::AfterCommitMarker)?;

        // 4 e 5. Aplica e limpa.
        self.apply(txdir, &plan)?;
        Ok(())
    }

    /// Aplica o plano de uma transação já commitada. **Idempotente**: pode rodar de novo depois
    /// de uma queda no meio (arquivo já movido = pula; já apagado = pula).
    fn apply(&self, txdir: &Path, plan: &Plan) -> Result<()> {
        let files = txdir.join(FILES);
        let mut done = 0usize;
        for rel in &plan.writes {
            let src = files.join(rel);
            if src.exists() {
                let dst = self.root.join(rel);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&src, &dst).with_context(|| format!("movendo {rel}"))?;
                if let Some(parent) = dst.parent() {
                    sync_dir(parent)?;
                }
            }
            done += 1;
            self.crash(Step_::AfterApply(done))?;
        }
        for rel in &plan.deletes {
            let dst = self.root.join(rel);
            match fs::remove_file(&dst) {
                Ok(()) => {
                    if let Some(parent) = dst.parent() {
                        sync_dir(parent)?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("apagando {rel}")),
            }
            done += 1;
            self.crash(Step_::AfterApply(done))?;
        }
        self.crash(Step_::BeforeCleanup)?;
        fs::remove_dir_all(txdir)?;
        if let Some(parent) = txdir.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    }

    /// Resolve transações interrompidas: com `COMMIT` → termina; sem → descarta.
    fn recover(&self) -> Result<()> {
        let staging = self.root.join(STAGING);
        let Ok(entries) = fs::read_dir(&staging) else {
            return Ok(());
        };
        let mut txdirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        txdirs.sort(); // ULID: ordem cronológica
        for txdir in txdirs {
            if txdir.join(COMMIT).exists() {
                let raw =
                    fs::read(txdir.join(PLAN)).context("lendo PLAN.json de transação commitada")?;
                let plan: Plan = serde_json::from_slice(&raw).context("PLAN.json inválido")?;
                info!(tx = %txdir.display(), "infra: terminando transação interrompida (roll-forward)");
                self.apply(&txdir, &plan)?;
            } else {
                info!(tx = %txdir.display(), "infra: descartando transação sem COMMIT");
                fs::remove_dir_all(&txdir)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_crash(&self, step: Step) {
        *self.crash_at.lock().unwrap() = Some(step);
    }

    #[cfg(test)]
    fn crash(&self, step: Step_) -> Result<()> {
        let want = *self.crash_at.lock().unwrap();
        if want == Some(step) {
            return Err(anyhow::Error::new(InjectedCrash(step)));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn crash(&self, _step: Step_) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
type Step_ = Step;
/// Em produção `Step` não existe; este alias vazio mantém as chamadas `self.crash(...)` iguais.
#[cfg(not(test))]
#[allow(non_camel_case_types, dead_code)]
#[derive(Clone, Copy)]
enum Step_ {
    AfterStagingFiles,
    AfterPlan,
    AfterCommitMarker,
    AfterApply(usize),
    BeforeCleanup,
}

/// `rel` precisa ser relativo, sem `..`, sem componente vazio e sem componente começando por `.`
/// (reservado para `.lock`, `.staging`, `.git`).
fn check_rel(rel: &str) -> Result<()> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        bail!("caminho inválido: {rel:?}");
    }
    for part in rel.split('/') {
        if part.is_empty() || part.starts_with('.') {
            bail!("caminho inválido (componente vazio ou começando por '.'): {rel:?}");
        }
    }
    Ok(())
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::create(path).with_context(|| format!("criando {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("fsync do diretório {}", path.display()))
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(tree: &FileTree, rel: &str) -> Option<String> {
        tree.read(rel)
            .unwrap()
            .map(|b| String::from_utf8(b).unwrap())
    }

    fn seed(tree: &FileTree) {
        let mut tx = Tx::new();
        tx.write("a.yml", "a1")
            .write("b.yml", "b1")
            .write("sub/c.yml", "c1");
        tree.commit(tx).unwrap();
    }

    fn the_tx() -> Tx {
        let mut tx = Tx::new();
        tx.write("a.yml", "a2")
            .write("sub/d.yml", "d2")
            .write("e.yml", "e2")
            .delete("sub/c.yml");
        tx
    }

    fn old_state(tree: &FileTree) -> bool {
        text(tree, "a.yml").as_deref() == Some("a1")
            && text(tree, "b.yml").as_deref() == Some("b1")
            && text(tree, "sub/c.yml").as_deref() == Some("c1")
            && text(tree, "sub/d.yml").is_none()
            && text(tree, "e.yml").is_none()
    }

    fn new_state(tree: &FileTree) -> bool {
        text(tree, "a.yml").as_deref() == Some("a2")
            && text(tree, "b.yml").as_deref() == Some("b1")
            && text(tree, "sub/c.yml").is_none()
            && text(tree, "sub/d.yml").as_deref() == Some("d2")
            && text(tree, "e.yml").as_deref() == Some("e2")
    }

    #[test]
    fn commit_grava_le_lista_e_apaga() {
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path()).unwrap();
        seed(&tree);
        assert_eq!(tree.list().unwrap(), vec!["a.yml", "b.yml", "sub/c.yml"]);
        tree.commit(the_tx()).unwrap();
        assert!(new_state(&tree));
        assert_eq!(
            tree.list().unwrap(),
            vec!["a.yml", "b.yml", "e.yml", "sub/d.yml"]
        );
        assert!(
            !dir.path().join(STAGING).read_dir().unwrap().any(|_| true),
            "staging sobrou"
        );
    }

    #[test]
    fn transacao_vazia_nao_faz_nada() {
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path()).unwrap();
        tree.commit(Tx::new()).unwrap();
        assert!(tree.list().unwrap().is_empty());
    }

    #[test]
    fn caminhos_invalidos_sao_recusados_sem_escrever_nada() {
        let dir = tempfile::tempdir().unwrap();
        let tree = FileTree::open(dir.path()).unwrap();
        for bad in [
            "",
            "/etc/passwd",
            "../fora",
            "a/../b",
            ".lock",
            "x/.git/y",
            "a//b",
            "a\\b",
        ] {
            let mut tx = Tx::new();
            tx.write("ok.yml", "x").write(bad, "y");
            assert!(tree.commit(tx).is_err(), "{bad:?} devia ser recusado");
        }
        assert!(
            tree.list().unwrap().is_empty(),
            "uma transação recusada escreveu algo"
        );
    }

    #[test]
    fn segunda_instancia_no_mesmo_diretorio_e_recusada() {
        let dir = tempfile::tempdir().unwrap();
        let _first = FileTree::open(dir.path()).unwrap();
        let err = FileTree::open(dir.path())
            .err()
            .expect("devia recusar")
            .to_string();
        assert!(err.contains("já está em uso"), "{err}");
    }

    /// O teste que importa: matar o processo em CADA ponto e conferir "tudo ou nada".
    #[test]
    fn queda_em_qualquer_ponto_deixa_estado_antigo_inteiro_ou_novo_inteiro() {
        // (ponto da queda, o estado esperado depois da recuperação: true = novo)
        let cases: &[(Step, bool)] = &[
            (Step::AfterStagingFiles, false),
            (Step::AfterPlan, false),
            (Step::AfterCommitMarker, true),
            (Step::AfterApply(1), true),
            (Step::AfterApply(2), true),
            (Step::AfterApply(3), true),
            (Step::AfterApply(4), true), // 3 writes + 1 delete = fim do plano
            (Step::BeforeCleanup, true),
        ];
        for &(step, expect_new) in cases {
            let dir = tempfile::tempdir().unwrap();
            {
                let tree = FileTree::open(dir.path()).unwrap();
                seed(&tree);
                tree.inject_crash(step);
                let err = tree.commit(the_tx()).unwrap_err();
                assert!(
                    err.downcast_ref::<InjectedCrash>().is_some(),
                    "{step:?}: {err}"
                );
            } // drop = processo "morreu" e o lock foi solto

            let tree = FileTree::open(dir.path()).unwrap(); // recovery
            if expect_new {
                assert!(
                    new_state(&tree),
                    "{step:?}: devia terminar no estado NOVO inteiro"
                );
            } else {
                assert!(
                    old_state(&tree),
                    "{step:?}: devia voltar ao estado ANTIGO inteiro"
                );
            }
            assert!(
                fs::read_dir(dir.path().join(STAGING))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
                "{step:?}: staging não foi limpo"
            );

            // E o repositório continua utilizável.
            let mut tx = Tx::new();
            tx.write("depois.yml", "ok");
            tree.commit(tx).unwrap();
            assert_eq!(text(&tree, "depois.yml").as_deref(), Some("ok"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn diretorio_de_infra_e_so_do_dono() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let _t = FileTree::open(d.path().join("infra")).unwrap();
        let mode = fs::metadata(d.path().join("infra"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn recuperacao_e_idempotente_mesmo_com_queda_durante_a_propria_recuperacao() {
        let dir = tempfile::tempdir().unwrap();
        {
            let tree = FileTree::open(dir.path()).unwrap();
            seed(&tree);
            tree.inject_crash(Step::AfterApply(1));
            tree.commit(the_tx()).unwrap_err();
        }
        // 1ª recuperação também "cai" no meio.
        {
            let raw = FileTree {
                root: dir.path().to_path_buf(),
                _lock: File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(dir.path().join(LOCK))
                    .unwrap(),
                commit_gate: Mutex::new(()),
                crash_at: Mutex::new(Some(Step::AfterApply(2))),
            };
            assert!(raw.recover().is_err());
        }
        let tree = FileTree::open(dir.path()).unwrap();
        assert!(new_state(&tree));
    }
}
