//! Cooperative revocation boundary for Hosted durable writes.
//!
//! Public Loomem never installs this task-local phase. Hosted aborts work only
//! before persistence begins; an accepted durable section finishes its version
//! links before revocation is acknowledged.
// The public binary compiles this module with the feature enabled, but only
// the Hosted library target (and its tests in the binary) drive the phases.
#![allow(dead_code)]
use anyhow::{bail, Result};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Pending,
    Durable,
    Revoked,
}

pub(crate) struct WritePhase {
    phase: Mutex<Phase>,
}

impl WritePhase {
    pub(crate) fn new() -> Self {
        Self {
            phase: Mutex::new(Phase::Pending),
        }
    }

    fn enter_durable(&self) -> Result<()> {
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| anyhow::anyhow!("Hosted write phase lock poisoned"))?;
        if *phase == Phase::Revoked {
            bail!("scope revoked before durable write")
        }
        *phase = Phase::Durable;
        Ok(())
    }

    /// Returns true when the task is still safe to abort. Poison conservatively
    /// drains the task rather than risking a partial durable write.
    pub(crate) fn revoke(&self) -> bool {
        match self.phase.lock() {
            Ok(phase) if *phase == Phase::Durable => false,
            Ok(mut phase) => {
                *phase = Phase::Revoked;
                true
            }
            Err(_) => false,
        }
    }
}

tokio::task_local! {
    static WRITE_PHASE: Arc<WritePhase>;
}

pub(crate) async fn track<F: std::future::Future>(phase: Arc<WritePhase>, future: F) -> F::Output {
    WRITE_PHASE.scope(phase, future).await
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

    #[test]
    fn pending_write_is_revocable_but_durable_write_must_finish() {
        let pending = WritePhase::new();
        assert!(pending.revoke());
        assert!(pending.enter_durable().is_err());

        let durable = WritePhase::new();
        durable.enter_durable().expect("enter durable phase");
        assert!(!durable.revoke());
    }
}
