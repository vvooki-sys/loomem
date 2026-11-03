//! Cooperative revocation boundary for Hosted durable writes.
//!
//! Public Loomem never installs this task-local phase. Hosted aborts work only
//! before persistence begins; an accepted durable section finishes its version
//! links before revocation is acknowledged. Multi-step background writes
//! (dream consolidation) open one short apply section per subject through
//! `loomem_core::apply_guard`; revocation waits for a section in progress and
//! stops the job at its next checkpoint.
// The public binary compiles this module with the feature enabled, but only
// the Hosted library target (and its tests in the binary) drive the phases.
#![allow(dead_code)]
use anyhow::{bail, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Pending,
    Applying,
    Durable,
    Revoked,
}

struct State {
    phase: Phase,
    revoke_requested: bool,
}

impl State {
    fn revoked(&self) -> bool {
        self.phase == Phase::Revoked || self.revoke_requested
    }
}

type ApplyHold = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub(crate) struct WritePhase {
    state: Mutex<State>,
    /// Evaluation-only: awaited once inside the first apply section so a test
    /// can revoke while the section is open.
    apply_hold: tokio::sync::Mutex<Option<ApplyHold>>,
}

impl WritePhase {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                phase: Phase::Pending,
                revoke_requested: false,
            }),
            apply_hold: tokio::sync::Mutex::new(None),
        }
    }

    pub(crate) fn with_apply_hold(self, hold: ApplyHold) -> Self {
        Self {
            apply_hold: tokio::sync::Mutex::new(Some(hold)),
            ..self
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("Hosted write phase lock poisoned"))
    }

    fn enter_durable(&self) -> Result<()> {
        let mut state = self.lock()?;
        if state.revoked() {
            bail!("scope revoked before durable write")
        }
        state.phase = Phase::Durable;
        Ok(())
    }

    /// Returns true when the task is still safe to abort. An open apply
    /// section or a durable write must finish first; the task then stops at
    /// its next checkpoint. Poison conservatively drains the task rather than
    /// risking a partial durable write.
    pub(crate) fn revoke(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        match state.phase {
            Phase::Durable => false,
            Phase::Applying => {
                state.revoke_requested = true;
                false
            }
            Phase::Pending | Phase::Revoked => {
                state.phase = Phase::Revoked;
                true
            }
        }
    }

    fn begin_apply(&self) -> Result<()> {
        let mut state = self.lock()?;
        if state.revoked() {
            bail!("scope revoked before apply section")
        }
        if state.phase == Phase::Pending {
            state.phase = Phase::Applying;
        }
        Ok(())
    }

    fn end_apply(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.phase == Phase::Applying {
                state.phase = if state.revoke_requested {
                    Phase::Revoked
                } else {
                    Phase::Pending
                };
            }
        }
    }

    fn checkpoint(&self) -> Result<()> {
        if self.lock()?.revoked() {
            bail!("scope revoked; stopping before the next apply section")
        }
        Ok(())
    }
}

impl loomem_core::apply_guard::ApplyGuard for WritePhase {
    fn begin(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.begin_apply()?;
            let hold = self.apply_hold.lock().await.take();
            if let Some(hold) = hold {
                hold.await?;
            }
            Ok(())
        })
    }

    fn end(&self) {
        self.end_apply();
    }

    fn checkpoint(&self) -> Result<()> {
        WritePhase::checkpoint(self)
    }
}

tokio::task_local! {
    static WRITE_PHASE: Arc<WritePhase>;
}

/// Installs `phase` as both the durable-write phase and the apply guard.
pub(crate) async fn track<F: std::future::Future>(phase: Arc<WritePhase>, future: F) -> F::Output {
    let guard: Arc<dyn loomem_core::apply_guard::ApplyGuard> = phase.clone();
    WRITE_PHASE
        .scope(phase, loomem_core::apply_guard::with_guard(guard, future))
        .await
}

pub(crate) fn enter_durable() -> Result<()> {
    match WRITE_PHASE.try_with(|phase| phase.enter_durable()) {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::apply_guard::ApplyGuard;

    #[test]
    fn pending_write_is_revocable_but_durable_write_must_finish() {
        let pending = WritePhase::new();
        assert!(pending.revoke());
        assert!(pending.enter_durable().is_err());

        let durable = WritePhase::new();
        durable.enter_durable().expect("enter durable phase");
        assert!(!durable.revoke());
    }

    #[tokio::test]
    async fn open_apply_section_defers_revocation_to_the_next_checkpoint() {
        let phase = WritePhase::new();
        phase.begin().await.expect("section opens");
        assert!(!phase.revoke(), "an open section must finish");
        assert!(phase.enter_durable().is_err());
        phase.end();
        assert!(WritePhase::checkpoint(&phase).is_err());
        assert!(phase.begin().await.is_err());
        assert!(phase.revoke(), "between sections the task is abortable");
    }

    #[tokio::test]
    async fn sections_track_the_installed_phase() {
        let phase = Arc::new(WritePhase::new());
        track(phase.clone(), async {
            let section = loomem_core::apply_guard::begin_section()
                .await
                .expect("section opens");
            assert!(!WRITE_PHASE.with(|phase| phase.revoke()));
            drop(section);
            assert!(loomem_core::apply_guard::checkpoint().is_err());
        })
        .await;
    }
}
