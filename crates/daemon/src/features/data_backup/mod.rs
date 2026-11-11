//! Backup dos **dados** dos serviços (volumes e bancos) para um S3. Plano e fases em
//! `docs/plano-backup-de-dados-para-s3.md`. Não confundir com `features/backup`, que guarda
//! snapshots das variáveis de ambiente.
//!
//! - [`destination`]: o destino S3 (um por servidor), gravado em `settings.yml` com a chave cifrada.

pub mod destination;
pub mod policy;
pub mod recover;
pub mod scheduler;
pub mod service_backup;
