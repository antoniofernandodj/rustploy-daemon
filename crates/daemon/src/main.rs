//! Ponto de entrada do `rustployd`: delega tudo à montagem do sistema em `bootstrap`.

mod api;
mod bootstrap;
// `domain` e `protocol` vieram de uma lib, onde tipo público sem uso não gerava aviso. Num binário
// gera — o allow segura isso até alguém podar o que sobrou.
#[allow(dead_code, unused_imports)]
mod domain;
mod engine;
mod features;
mod platform;
#[allow(dead_code, unused_imports)]
mod protocol;
mod store;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bootstrap::run().await
}
