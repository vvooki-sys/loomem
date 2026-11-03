//! Registry for fire-and-forget work whose writes must precede generation seal.
use std::future::Future;
use std::sync::{Mutex, MutexGuard};
use tokio::task::{AbortHandle, JoinError, JoinSet};

#[cfg(feature = "hosted-engine")]
use crate::handlers::ingest::write_phase::{self, WritePhase};
#[cfg(feature = "hosted-engine")]
use std::sync::Arc;

/// Logged when work is tracked after revocation began; tests sequence on it.
#[cfg(feature = "hosted-engine")]
pub(crate) const DEFERRED_DROPPED_MARKER: &str = "LOOMEM_HOSTED_DEFERRED_DROPPED";
/// Logged once `begin_abort` has closed the registry; tests sequence on it.
#[cfg(feature = "hosted-engine")]
pub(crate) const DEFERRED_CLOSED_MARKER: &str = "LOOMEM_HOSTED_DEFERRED_CLOSED";

#[derive(Default)]
struct Inner {
    tasks: JoinSet<()>,
    /// Hosted only: the revocation phase of every tracked task.
    #[cfg(feature = "hosted-engine")]
    phases: Vec<(AbortHandle, Arc<WritePhase>)>,
    /// Hosted only: set by `begin_abort` and never cleared. A job that was
    /// still finishing a durable write when revocation began may track new
    /// work afterwards (an auto-dream); that work is dropped unstarted.
    #[cfg(feature = "hosted-engine")]
    closed: bool,
}

impl Inner {
    /// Takes the outstanding work, keeping the registry's closed state.
    fn take_work(&mut self) -> Inner {
        #[cfg(feature = "hosted-engine")]
        let closed = self.closed;
        let work = std::mem::take(self);
        #[cfg(feature = "hosted-engine")]
        {
            self.closed = closed;
        }
        work
    }
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
            if inner.closed {
                tracing::info!(
                    "deferred work tracked after scope revocation began; dropped unstarted ({DEFERRED_DROPPED_MARKER})"
                );
                return;
            }
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
        let mut inner = self.lock().take_work();
        while let Some(result) = inner.tasks.join_next().await {
            result?;
        }
        Ok(())
    }

    /// Cancel old-epoch fire-and-forget work during Hosted revocation. A task
    /// inside an apply section or a durable write is not aborted; it finishes
    /// that section and stops at its next checkpoint, and this waits for it.
    pub async fn abort(&self) {
        self.begin_abort().join().await;
    }

    /// Signals every tracked task without waiting, so a caller revoking more
    /// than one kind of work can signal all of it before joining any. In
    /// Hosted it also closes the registry: work tracked afterwards, e.g. by a
    /// job still finishing its durable write, never starts.
    pub fn begin_abort(&self) -> AbortingTasks {
        let inner = {
            let mut guard = self.lock();
            #[cfg(feature = "hosted-engine")]
            {
                guard.closed = true;
                tracing::info!(
                    "deferred registry closed for revocation ({DEFERRED_CLOSED_MARKER})"
                );
            }
            guard.take_work()
        };
        #[cfg(feature = "hosted-engine")]
        for (handle, phase) in &inner.phases {
            if phase.revoke() {
                handle.abort();
            }
        }
        #[cfg(not(feature = "hosted-engine"))]
        let inner = {
            let mut inner = inner;
            inner.tasks.abort_all();
            inner
        };
        AbortingTasks(inner.tasks)
    }
}

/// Tasks already signalled by [`DeferredTasks::begin_abort`].
pub struct AbortingTasks(JoinSet<()>);

impl AbortingTasks {
    /// Waits for aborted tasks and for protected sections to finish.
    pub async fn join(mut self) {
        while self.0.join_next().await.is_some() {}
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_waits_for_an_open_apply_section_and_stops_after_it() {
        let tasks = DeferredTasks::default();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let applied = Arc::new(AtomicUsize::new(0));
        let observed = applied.clone();
        tasks.track(async move {
            let section = loomem_core::apply_guard::begin_section()
                .await
                .expect("first section opens");
            let _ = entered_tx.send(());
            // Inside the section a checkpoint fails only once revocation was
            // requested, so the section provably outlives the abort call.
            while loomem_core::apply_guard::checkpoint().is_ok() {
                tokio::task::yield_now().await;
            }
            observed.fetch_add(1, Ordering::SeqCst);
            drop(section);
            if loomem_core::apply_guard::checkpoint().is_ok() {
                observed.fetch_add(100, Ordering::SeqCst);
            }
        });
        entered_rx.await.expect("section entered");
        tasks.abort().await;
        assert_eq!(
            applied.load(Ordering::SeqCst),
            1,
            "section finished after revocation, next checkpoint stopped the task"
        );
    }

    #[cfg(feature = "hosted-engine")]
    #[tokio::test]
    async fn work_tracked_after_revocation_began_never_starts() {
        let tasks = DeferredTasks::default();
        let aborting = tasks.begin_abort();
        let started = Arc::new(AtomicUsize::new(0));
        let observed = started.clone();
        tasks.track(async move {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        aborting.join().await;
        tasks.drain().await.expect("nothing to drain");
        assert_eq!(started.load(Ordering::SeqCst), 0);
        // Draining does not reopen a revoked registry.
        tasks.track(async {});
        assert!(tasks.lock().tasks.is_empty());
    }
}
