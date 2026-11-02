//! O **repositório** do estado de infra: o `InfraState` em memória (as leituras vêm daqui) mais a
//! pasta de arquivos YAML (a persistência). Plano: `docs/plano-backup-estado-em-git.md`, R3.
//!
//! Toda escrita passa por [`Repo::update`]: copia o estado, aplica a mudança, **valida tudo** e só
//! então grava numa transação (`txn`). Se qualquer passo falhar, nada muda — nem no disco nem na
//! memória. As leituras ([`Repo::snapshot`]) nunca veem um estado pela metade.

use super::model::InfraState;
use super::txn::FileTree;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

pub struct Repo {
    tree: FileTree,
    /// Contador que sobe a cada escrita que mudou o estado: quem versiona o diretório (git) acorda
    /// por aqui, sem precisar vigiar o disco.
    changes: tokio::sync::watch::Sender<u64>,
    state: RwLock<Arc<InfraState>>,
    /// Serializa os escritores (a leitura não toma este lock).
    gate: Mutex<()>,
    /// Testes: a pasta temporária vive enquanto o repositório viver.
    #[cfg(test)]
    _tmp: Option<tempfile::TempDir>,
}

impl Repo {
    /// Abre (e recupera de queda, se for o caso) a pasta e carrega o estado. Pasta sem
    /// `MANIFEST.json` = ainda não há infra gravada: o estado começa vazio.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let tree = FileTree::open(dir)?;
        let state = InfraState::read(&tree)?;
        Ok(Self {
            tree,
            changes: tokio::sync::watch::channel(0).0,
            state: RwLock::new(Arc::new(state.unwrap_or_default())),
            gate: Mutex::new(()),
            #[cfg(test)]
            _tmp: None,
        })
    }

    /// Repositório numa pasta temporária que some junto com ele.
    #[cfg(test)]
    pub fn temp() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = Self::open(tmp.path().join("infra")).unwrap();
        r._tmp = Some(tmp);
        r
    }

    pub fn tree(&self) -> &FileTree {
        &self.tree
    }

    /// Acorda quando o estado muda (o valor é só um contador crescente).
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Roda `f` com o **lock de escrita tomado**: nenhuma transação está no meio e nenhuma começa
    /// até `f` terminar. É o único jeito seguro de deixar um processo externo (o `git`) ler o
    /// diretório — R3: "só enxerga o diretório quando a transação já terminou". `f` recebe a raiz
    /// do diretório e o estado atual. Mantenha `f` curto.
    pub fn exclusive<R>(&self, f: impl FnOnce(&std::path::Path, &InfraState) -> R) -> R {
        let _g = self.gate.lock().unwrap();
        let state = self.state.read().unwrap().clone();
        f(self.tree.root(), &state)
    }

    /// Já existe estado gravado no disco (`MANIFEST.json`)?
    pub fn is_initialized(&self) -> Result<bool> {
        Ok(self.tree.read("MANIFEST.json")?.is_some())
    }

    /// O estado atual. Barato (um `Arc`); quem segura uma cópia antiga não atrapalha ninguém.
    pub fn snapshot(&self) -> Arc<InfraState> {
        self.state.read().unwrap().clone()
    }

    /// Aplica uma **árvore inteira vinda de fora** (o `pull` do git) como UMA transação: lê com
    /// [`InfraState::from_files`], valida, deixa `check` recusar, grava **os bytes exatos** que
    /// vieram (para o disco ficar idêntico ao commit — senão reserializar geraria um diff e um
    /// commit de volta, o laço que a R6 proíbe), apaga os projetos que sumiram e só então troca o
    /// estado em memória. Tudo com o lock de escrita tomado. `check(raiz, antigo, novo)` roda antes
    /// de gravar qualquer coisa; `finish(raiz)` roda depois de gravar (ex.: mover o HEAD do git) e,
    /// se falhar, o erro volta mas o estado já está aplicado. Devolve `(antigo, novo)`.
    pub fn apply_files(
        &self,
        files: &std::collections::BTreeMap<String, Vec<u8>>,
        check: impl FnOnce(&std::path::Path, &InfraState, &InfraState) -> Result<()>,
        finish: impl FnOnce(&std::path::Path) -> Result<()>,
    ) -> Result<(Arc<InfraState>, Arc<InfraState>)> {
        use super::model::is_state_file;
        let _g = self.gate.lock().unwrap();
        let new = InfraState::from_files(files)?
            .ok_or_else(|| anyhow::anyhow!("a árvore recebida não tem MANIFEST.json"))?;
        new.validate()?;
        let old = self.state.read().unwrap().clone();
        check(self.tree.root(), &old, &new)?;

        let mut tx = super::txn::Tx::new();
        for (rel, bytes) in files.iter().filter(|(r, _)| is_state_file(r)) {
            if self.tree.read(rel)?.as_deref() != Some(bytes.as_slice()) {
                tx.write(rel.clone(), bytes.clone());
            }
        }
        for rel in self.tree.list()? {
            if rel.starts_with("projects/") && is_state_file(&rel) && !files.contains_key(&rel) {
                tx.delete(rel);
            }
        }
        self.tree.commit(tx)?;
        let new = Arc::new(new);
        *self.state.write().unwrap() = new.clone();
        self.changes.send_modify(|v| *v += 1);
        finish(self.tree.root())?;
        Ok((old, new))
    }

    /// Relê a pasta do disco, **valida** e só então troca o estado em memória. É o que "retomar" faz
    /// depois de alguém ter mexido nos arquivos à mão (um `merge` por SSH, por exemplo): o daemon
    /// guarda o estado em memória e, sem isto, a próxima escrita sobrescreveria o que a pessoa
    /// resolveu. Se a pasta não for um estado válido (marcador de conflito no YAML, referência
    /// quebrada…), devolve o erro e **nada muda**. Devolve `(antigo, novo)`.
    pub fn reload_checked(&self) -> Result<(Arc<InfraState>, Arc<InfraState>)> {
        let _g = self.gate.lock().unwrap();
        let new = InfraState::read(&self.tree)?
            .ok_or_else(|| anyhow::anyhow!("a pasta não tem MANIFEST.json"))?;
        new.validate()?;
        let old = self.state.read().unwrap().clone();
        let new = Arc::new(new);
        if *old != *new {
            *self.state.write().unwrap() = new.clone();
            self.changes.send_modify(|v| *v += 1);
        }
        Ok((old, new))
    }

    /// Substitui o estado em memória pelo que está no disco (depois da migração).
    pub fn reload(&self) -> Result<()> {
        let _g = self.gate.lock().unwrap();
        let st = InfraState::read(&self.tree)?.unwrap_or_default();
        *self.state.write().unwrap() = Arc::new(st);
        self.changes.send_modify(|v| *v += 1);
        Ok(())
    }

    /// Aplica `f` ao estado e grava. `f` devolvendo `Err` aborta sem efeito.
    pub fn update<R>(&self, f: impl FnOnce(&mut InfraState) -> Result<R>) -> Result<R> {
        let _g = self.gate.lock().unwrap();
        let mut next = (**self.state.read().unwrap()).clone();
        let out = f(&mut next)?;
        next.normalize();
        if next == **self.state.read().unwrap() && self.is_initialized()? {
            return Ok(out);
        }
        next.write(&self.tree)?;
        *self.state.write().unwrap() = Arc::new(next);
        self.changes.send_modify(|v| *v += 1);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::infra::model::SettingsInfra;

    #[test]
    fn update_grava_e_sobrevive_a_reabrir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("infra");
        {
            let r = Repo::open(&dir).unwrap();
            assert!(!r.is_initialized().unwrap());
            r.update(|s| {
                s.settings = SettingsInfra {
                    acme_email: Some("a@b.c".into()),
                    ..Default::default()
                };
                Ok(())
            })
            .unwrap();
            assert_eq!(r.snapshot().settings.acme_email.as_deref(), Some("a@b.c"));
        }
        let r = Repo::open(&dir).unwrap();
        assert_eq!(r.snapshot().settings.acme_email.as_deref(), Some("a@b.c"));
    }

    #[test]
    fn erro_na_mudanca_ou_na_validacao_nao_deixa_rastro() {
        let r = Repo::temp();
        r.update(|s| {
            s.settings.acme_email = Some("antes".into());
            Ok(())
        })
        .unwrap();
        // a closure falha
        let falha: Result<()> = r.update(|s| {
            s.settings.acme_email = Some("depois".into());
            anyhow::bail!("não")
        });
        assert!(falha.is_err());
        // a validação falha (dois projetos com o mesmo nome)
        let e: Result<()> = r.update(|s| {
            for id in ["a", "b"] {
                s.projects.push(crate::store::infra::model::ProjectInfra {
                    id: id.into(),
                    name: "igual".into(),
                    description: None,
                    env_vars: vec![],
                    env_comments: vec![],
                    network_name: None,
                    created_at: chrono::Utc::now(),
                    services: vec![],
                    jobs: vec![],
                    secrets: vec![],
                    managed_databases: vec![],
                    shared_access: vec![],
                });
            }
            Ok(())
        });
        assert!(e.is_err());
        assert_eq!(r.snapshot().settings.acme_email.as_deref(), Some("antes"));
        assert!(r.snapshot().projects.is_empty());
        let relido = InfraState::read(r.tree()).unwrap().unwrap();
        assert!(relido.projects.is_empty());
    }

    #[tokio::test]
    async fn escrita_efetiva_avisa_quem_assina_e_escrita_igual_nao() {
        let r = Repo::temp();
        let mut rx = r.subscribe();
        r.update(|s| {
            s.settings.acme_email = Some("a@b.c".into());
            Ok(())
        })
        .unwrap();
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        // a mesma mudança de novo não muda o estado → ninguém é acordado
        r.update(|s| {
            s.settings.acme_email = Some("a@b.c".into());
            Ok(())
        })
        .unwrap();
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn exclusive_ve_a_raiz_e_o_estado_e_segura_os_escritores() {
        let r = Repo::temp();
        r.update(|s| {
            s.settings.acme_email = Some("x".into());
            Ok(())
        })
        .unwrap();
        let (existe, email) = r.exclusive(|root, st| {
            (
                root.join("MANIFEST.json").exists(),
                st.settings.acme_email.clone(),
            )
        });
        assert!(existe);
        assert_eq!(email.as_deref(), Some("x"));
    }
}
