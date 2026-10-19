//! Cache de um valor só, com TTL, que junta chamadas concorrentes numa só ida à fonte lenta.

use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

/// Single-slot value cache with a TTL. The async lock is held across a
/// miss-refresh so concurrent callers coalesce into ONE upstream call instead
/// of a thundering herd — the whole point, since the upstream here is a slow
/// `docker system df`.
pub struct TtlCache<T> {
    ttl: Duration,
    slot: AsyncMutex<Option<(Instant, T)>>,
}

impl<T: Clone> TtlCache<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slot: AsyncMutex::new(None),
        }
    }

    /// Returns the cached value if still within the TTL, otherwise runs
    /// `refresh`, stores and returns it. On a `refresh` error any previously
    /// cached (now stale) value is left untouched and the error is propagated.
    pub async fn get_or_refresh<F, Fut, E>(&self, refresh: F) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let mut slot = self.slot.lock().await;
        if let Some((at, v)) = slot.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(v.clone());
        }
        let v = refresh().await?;
        *slot = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// Drops the cached value so the next `get_or_refresh` fetches fresh.
    pub async fn invalidate(&self) {
        *self.slot.lock().await = None;
    }
}
