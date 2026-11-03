//! Evaluation-only gate that keeps a scoped job in flight until released.
//!
//! The concurrency tests need a job that stays pending deterministically,
//! without sleeping. A `tool_async` request may name the synthetic failpoint
//! `hosted_hold`; the job then waits before dispatch until `release_holds`
//! arrives. The hold happens before any durable write, so `revoke` can still
//! abort the job. Without the `evaluation` feature the name is rejected like
//! any other unknown failpoint, so production binaries never hold anything.
use anyhow::Result;

#[cfg(feature = "evaluation")]
const HOLD_FAILPOINT: &str = "hosted_hold";

#[cfg(feature = "evaluation")]
pub(super) struct Holds {
    generation: tokio::sync::watch::Sender<u64>,
}

#[cfg(feature = "evaluation")]
impl Holds {
    pub(super) fn new() -> Self {
        let (generation, _) = tokio::sync::watch::channel(0);
        Self { generation }
    }

    /// Splits the synthetic hold off the failpoint name; the remainder still
    /// goes through the core failpoint parser.
    pub(super) fn gate<'a>(
        &self,
        failpoint: Option<&'a str>,
    ) -> (Option<HoldGate>, Option<&'a str>) {
        if failpoint != Some(HOLD_FAILPOINT) {
            return (None, failpoint);
        }
        let gate = HoldGate {
            receiver: self.generation.subscribe(),
            generation: *self.generation.borrow(),
        };
        (Some(gate), None)
    }

    /// Releases every job holding at the current generation.
    pub(super) fn release(&self) -> Result<u64> {
        self.generation.send_modify(|generation| *generation += 1);
        Ok(*self.generation.borrow())
    }
}

#[cfg(feature = "evaluation")]
pub(super) struct HoldGate {
    receiver: tokio::sync::watch::Receiver<u64>,
    generation: u64,
}

#[cfg(feature = "evaluation")]
impl HoldGate {
    pub(super) async fn wait(mut self) -> Result<()> {
        let held_at = self.generation;
        self.receiver
            .wait_for(|generation| *generation > held_at)
            .await
            .map_err(|_| anyhow::anyhow!("hold dropped with its engine"))?;
        Ok(())
    }
}

#[cfg(not(feature = "evaluation"))]
pub(super) struct Holds;

#[cfg(not(feature = "evaluation"))]
impl Holds {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn gate<'a>(
        &self,
        failpoint: Option<&'a str>,
    ) -> (Option<HoldGate>, Option<&'a str>) {
        (None, failpoint)
    }

    pub(super) fn release(&self) -> Result<u64> {
        anyhow::bail!("release_holds requires the evaluation feature")
    }
}

#[cfg(not(feature = "evaluation"))]
pub(super) struct HoldGate;

#[cfg(not(feature = "evaluation"))]
impl HoldGate {
    pub(super) async fn wait(self) -> Result<()> {
        Ok(())
    }
}
