//! Containers de serviços Application: nomes (live, staging, réplicas, legado),
//! busca por label, criação, start/stop, IP e alias de rede.

mod inspect;
mod lifecycle;
mod naming;
mod query;

pub use inspect::*;
pub use lifecycle::*;
pub use naming::*;
pub use query::*;

// ── Nomes de container de serviço Application ───────────────────────────────
//
// Formato atual: `rp_<id8>_<safe>` (+ `_r<i>` / `_staging_<dep>`), com a base
// de `shared::app_container_base`. Formato legado, de antes do ID no nome:
// `rp_<safe>` (+ os mesmos sufixos). Nenhum dos dois serve para **achar** o
// container de um serviço — isso é por label (`find_live_replicas`, e
// `find_owned_by_name` para staging). O legado só é montado para achar o
// staging de um deploy que estava em andamento quando o daemon foi atualizado.
// Ver `docs/plano-colisao-nome-container.md`.
