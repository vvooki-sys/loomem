//! Cooperative cancellation boundary for multi-step background writes.
//!
//! A long background job (dream consolidation applies one subject after
//! another) must not be torn down in the middle of a subject: the merged
//! chunk would be durable while its contradictions and embedding request
//! are not, and the idempotency check would then skip the subject forever.
//! A guard installed by the caller marks each short apply section; the
//! caller cancels only between sections and waits for one in progress.
//!
//! Public Loomem never installs a guard, so every function here degrades to
//! a task-local lookup that finds nothing and returns `Ok`.
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub trait ApplyGuard: Send + Sync {
    /// Opens a section that must finish once started. An error means
    /// cancellation was already requested and the section must not start.
    fn begin(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    /// Closes the section opened by `begin`.
    fn end(&self);
    /// Between sections: an error means stop before starting more work.
    fn checkpoint(&self) -> Result<()>;
}

tokio::task_local! {
    static APPLY_GUARD: Arc<dyn ApplyGuard>;
}

/// Runs `future` with `guard` installed for every section it opens.
pub async fn with_guard<F: Future>(guard: Arc<dyn ApplyGuard>, future: F) -> F::Output {
    APPLY_GUARD.scope(guard, future).await
}

/// Cancellation point between sections; `Ok` when no guard is installed.
pub fn checkpoint() -> Result<()> {
    APPLY_GUARD
        .try_with(|guard| guard.checkpoint())
        .unwrap_or(Ok(()))
}

/// Keeps a section open until dropped.
#[must_use = "the section closes when this value drops"]
pub struct ApplySection(Option<Arc<dyn ApplyGuard>>);

/// Opens a section on the installed guard, or a no-op section without one.
pub async fn begin_section() -> Result<ApplySection> {
    let Ok(guard) = APPLY_GUARD.try_with(Arc::clone) else {
        return Ok(ApplySection(None));
    };
    guard.begin().await?;
    Ok(ApplySection(Some(guard)))
}

impl Drop for ApplySection {
    fn drop(&mut self) {
        if let Some(guard) = &self.0 {
            guard.end();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        begun: AtomicUsize,
        ended: AtomicUsize,
        refuse: bool,
    }

    impl ApplyGuard for Counting {
        fn begin(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async move {
                if self.refuse {
                    anyhow::bail!("refused")
                }
                self.begun.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn end(&self) {
            self.ended.fetch_add(1, Ordering::SeqCst);
        }

        fn checkpoint(&self) -> Result<()> {
            if self.refuse {
                anyhow::bail!("stop")
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn without_a_guard_everything_is_a_no_op() {
        assert!(checkpoint().is_ok());
        let section = begin_section().await.expect("no guard installed");
        drop(section);
    }

    #[tokio::test]
    async fn installed_guard_sees_sections_and_checkpoints() {
        let guard = Arc::new(Counting {
            begun: AtomicUsize::new(0),
            ended: AtomicUsize::new(0),
            refuse: false,
        });
        let installed: Arc<dyn ApplyGuard> = guard.clone();
        with_guard(installed, async {
            assert!(checkpoint().is_ok());
            let section = begin_section().await.expect("section opens");
            drop(section);
        })
        .await;
        assert_eq!(guard.begun.load(Ordering::SeqCst), 1);
        assert_eq!(guard.ended.load(Ordering::SeqCst), 1);

        let refusing: Arc<dyn ApplyGuard> = Arc::new(Counting {
            begun: AtomicUsize::new(0),
            ended: AtomicUsize::new(0),
            refuse: true,
        });
        with_guard(refusing, async {
            assert!(checkpoint().is_err());
            assert!(begin_section().await.is_err());
        })
        .await;
    }
}
