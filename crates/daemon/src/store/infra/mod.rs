//! Camada de repositório da **infraestrutura** declarada (projetos, serviços, jobs…), que passa
//! a morar em YAML no disco em vez de no SQLite. Plano: `docs/plano-backup-estado-em-git.md`
//! (seções R2, R3 e R9 — Fase 1).
//!
//! Desde a Fase 4d os módulos de `store/` (projetos, serviços, jobs, secrets, providers…) leem e
//! escrevem aqui, não mais no SQLite; o SQL ficou só com o runtime.
//! - [`txn`]: um conjunto de arquivos com transação de "tudo ou nada" (lock, staging, marcador
//!   `COMMIT` e recuperação de queda).
//! - [`yaml`]: serialização determinística, para dois exports do mesmo estado darem bytes idênticos.
//! - [`model`]: o estado tipado e seu layout em arquivos.
//! - [`repo`]: o estado em memória + `update()` transacional; é o que `Db::infra()` entrega.
//! - [`migrate`]: a migração única SQL → YAML, que roda no `connect`.

pub mod audit;
pub mod migrate;
pub mod model;
pub mod repo;
pub mod txn;
pub mod yaml;
