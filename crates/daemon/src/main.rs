//! Ponto de entrada do `rustployd`: delega tudo à montagem do sistema em `bootstrap`.

// `Result<_, Response>`: um handler sai cedo devolvendo o `Response` que vira a resposta HTTP. O
// `Response` (o enum do protocolo, ~300 bytes) é movido uma vez por requisição; encaixotá-lo em doze
// lugares não economiza nada que se meça. O clippy do Rust 1.100 passou a acusar isso.
#![allow(clippy::result_large_err)]

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
