//! Verificação de versão: pergunta ao GitHub qual é a última release e compara com a do daemon.
//! Só leitura. Instalar a atualização é outra etapa (`docs/plano-updaters-daemon-e-gui.md`).

pub mod checker;
pub mod github_release;

pub use checker::UpdateChecker;
