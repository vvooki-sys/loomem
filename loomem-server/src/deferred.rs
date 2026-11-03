//! Registry for fire-and-forget work whose writes must precede generation seal.
use std::future::Future;
use std::sync::{Mutex, MutexGuard};
use tokio::task::{AbortHandle, JoinError, JoinSet};

#[cfg(feature = "hosted-engine")]
use crate::handlers::ingest::write_phase::{self, WritePhase};
#[cfg(feature = "hosted-engine")]
use std::sync::Arc;

#[derive(Default)]
struct Inner {
    tasks: JoinSet<()>,
    /// Hosted only: the revocation phase of every tracked task.
    #[cfg(feature = "hosted-engine")]
    phases: Vec<(AbortHandle, Arc<WritePhase>)>,
}

#[derive(Default)]
pub struct DeferredTasks {
    inner: Mutex<Inner>,
}

impl DeferredTasks {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn track(&self, future: impl Future<Output = ()> + Send + 'static) {
        let mut inner = self.lock();
        while inner.tasks.try_join_next().is_some() {}
        #[cfg(feature = "hosted-engine")]
        {
            inner.phases.retain(|(handle, _)| !handle.is_finished());
            let phase = Arc::new(WritePhase::new());
            let handle = inner.tasks.spawn(write_phase::track(phase.clone(), future));
            inner.phases.push((handle, phase));
        }
        #[cfg(not(feature = "hosted-engine"))]
        {
            let _: AbortHandle = inner.tasks.spawn(future);
        }
    }

    /// Take ownership of outstanding tasks so no lock is held across `.await`.
    pub async fn drain(&self) -> Result<(), JoinError> {
        let mut inner = std::mem::take(&mut *self.lock());
        while let Some(result) = inner.tasks.join_next().await {
            result?;
        }
        Ok(())
    }

    /// Cancel old-epoch fire-and-forget work during Hosted revocation. A task
    /// inside an apply section or a durable write is not aborted; it finishes
    /// that section and stops at its next checkpoint, and this waits for it.
    pub async fn abort(&self) {
        let mut inner = std::mem::take(&mut *self.lock());
        #[cfg(feature = "hosted-engine")]
        for (handle, phase) in &inner.phases {
            if phase.revoke() {
                handle.abort();
            }
        }
        #[cfg(not(feature = "hosted-engine"))]
        inner.tasks.abort_all();
        while inner.tasks.join_next().await.is_some() {}
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

    #[tokio::test]
    async fn abort_cancels_registered_work() {
        let tasks = DeferredTasks::default();
        let completed = Arc::new(AtomicUsize::new(0));
        let observed = completed.clone();
        tasks.track(async move {
            std::future::pending::<()>().await;
            observed.fetch_add(1, Ordering::SeqCst);
        });
        tasks.abort().await;
        assert_eq!(completed.load(Ordering::SeqCst), 0);
    }

    #[cfg(feature = "hosted-engine")]
    #[tokio::test]
    async fn abort_waits_for_an_open_apply_section_and_stops_after_it() {
        let tasks = DeferredTasks::default();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let applied = Arc::new(AtomicUsize::new(0));
        let observed = applied.clone();
        tasks.track(async move {
            let section = loomem_core::apply_guard::begin_section()
                .await
                .expect("first section opens");
            let _ = entered_tx.send(());
            let _ = release_rx.await;
            observed.fetch_add(1, Ordering::SeqCst);
            drop(section);
            if loomem_core::apply_guard::checkpoint().is_ok() {
                observed.fetch_add(100, Ordering::SeqCst);
            }
        });
        entered_rx.await.expect("section entered");
        let aborting = tokio::spawn(async move {
            tasks.abort().await;
        });
        let _ = release_tx.send(());
        aborting.await.expect("abort completes");
        assert_eq!(
            applied.load(Ordering::SeqCst),
            1,
            "section finished, next checkpoint stopped the task"
        );
    }
}
