//! Ponto de entrada do `rustployd`: delega tudo à montagem do sistema em `bootstrap`.

mod api;
mod bootstrap;
mod engine;
mod features;
mod platform;
mod store;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bootstrap::run().await
}
