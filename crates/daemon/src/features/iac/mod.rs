//! Infraestrutura como código: manifesto de projeto/servidor e pacote de serviço (export/import).

mod context;
pub mod manifest;
pub mod manifest_apply;
pub mod manifest_export;
pub mod manifest_export_all;
pub mod manifest_import;
pub mod service_bundle;
pub mod service_export;
pub mod service_import;

pub use context::IacContext;
