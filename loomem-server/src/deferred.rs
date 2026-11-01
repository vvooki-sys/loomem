//! Registry for fire-and-forget work whose writes must precede generation seal.
use std::future::Future;
use std::sync::{Mutex, MutexGuard};
use tokio::task::{JoinError, JoinSet};

#[derive(Default)]
pub struct DeferredTasks {
    tasks: Mutex<JoinSet<()>>,
}

impl DeferredTasks {
    fn lock(&self) -> MutexGuard<'_, JoinSet<()>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn track(&self, future: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.lock();
        while tasks.try_join_next().is_some() {}
        tasks.spawn(future);
    }

    /// Take ownership of outstanding tasks so no lock is held across `.await`.
    pub async fn drain(&self) -> Result<(), JoinError> {
        let mut tasks = std::mem::take(&mut *self.lock());
        while let Some(result) = tasks.join_next().await {
            result?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn drain_waits_for_registered_work() {
        let tasks = DeferredTasks::default();
        let completed = Arc::new(AtomicUsize::new(0));
        let observed = completed.clone();
        tasks.track(async move {
            tokio::task::yield_now().await;
            observed.fetch_add(1, Ordering::SeqCst);
        });
        tasks.drain().await.expect("deferred task succeeds");
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }
}
