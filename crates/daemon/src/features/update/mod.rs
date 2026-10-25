//! Verificação de versão: pergunta ao GitHub qual é a última release e compara com a do daemon.
//! `checker` só consulta; `apply` baixa e entrega ao helper root `rustployd-update`, que é quem
//! instala (`docs/plano-updaters-daemon-e-gui.md`).

pub mod apply;
pub mod checker;
pub mod github_release;

pub use apply::UpdateApplier;
pub use checker::UpdateChecker;
