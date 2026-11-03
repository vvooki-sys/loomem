//! Evaluation-only gates that keep a scoped job in flight until released.
//!
//! The concurrency tests need a job that stays pending deterministically,
//! without sleeping. A `tool_async` request may name one of three synthetic
//! failpoints: `hosted_hold` waits before dispatch, where `revoke` can still
//! abort the job; `hosted_hold_apply` waits inside the job's first apply
//! section (see `loomem_core::apply_guard`), and `hosted_hold_durable` waits
//! right after the job entered its durable phase; in both `revoke` must wait
//! for the job. All wait until `release_holds` arrives; the apply and durable
//! holds print `LOOMEM_HOSTED_HOLD_APPLY` / `LOOMEM_HOSTED_HOLD_DURABLE` on
//! stderr when they start waiting so an external test can sequence its
//! revocation. Without the `evaluation`
//! feature the names are rejected like any other unknown failpoint, so
//! production binaries never hold anything.
use anyhow::Result;

#[cfg(feature = "evaluation")]
const HOLD_FAILPOINT: &str = "hosted_hold";
#[cfg(feature = "evaluation")]
const APPLY_HOLD_FAILPOINT: &str = "hosted_hold_apply";
#[cfg(feature = "evaluation")]
const APPLY_HOLD_MARKER: &str = "LOOMEM_HOSTED_HOLD_APPLY";
#[cfg(feature = "evaluation")]
const DURABLE_HOLD_FAILPOINT: &str = "hosted_hold_durable";
#[cfg(feature = "evaluation")]
const DURABLE_HOLD_MARKER: &str = "LOOMEM_HOSTED_HOLD_DURABLE";

/// Where a job waits for release.
#[cfg_attr(not(feature = "evaluation"), allow(dead_code))]
pub(super) enum HoldPoint {
    None,
    BeforeDispatch(HoldGate),
    InsideApply(HoldGate),
    AfterDurable(HoldGate),
}

#[cfg(feature = "evaluation")]
pub struct Holds {
    generation: tokio::sync::watch::Sender<u64>,
}

#[cfg(feature = "evaluation")]
impl Holds {
    pub(super) fn new() -> Self {
        let (generation, _) = tokio::sync::watch::channel(0);
        Self { generation }
    }

    /// Splits the synthetic holds off the failpoint name; the remainder still
    /// goes through the core failpoint parser.
    pub(super) fn gate<'a>(&self, failpoint: Option<&'a str>) -> (HoldPoint, Option<&'a str>) {
        match failpoint {
            Some(HOLD_FAILPOINT) => (HoldPoint::BeforeDispatch(self.new_gate(None)), None),
            Some(APPLY_HOLD_FAILPOINT) => (
                HoldPoint::InsideApply(self.new_gate(Some(APPLY_HOLD_MARKER))),
                None,
            ),
            Some(DURABLE_HOLD_FAILPOINT) => (
                HoldPoint::AfterDurable(self.new_gate(Some(DURABLE_HOLD_MARKER))),
                None,
            ),
            other => (HoldPoint::None, other),
        }
    }

    fn new_gate(&self, marker: Option<&'static str>) -> HoldGate {
        HoldGate {
            receiver: self.generation.subscribe(),
            generation: *self.generation.borrow(),
            marker,
        }
    }

    /// Releases every job holding at the current generation.
    pub fn release(&self) -> Result<u64> {
        self.generation.send_modify(|generation| *generation += 1);
        Ok(*self.generation.borrow())
    }
}

#[cfg(feature = "evaluation")]
pub struct HoldGate {
    receiver: tokio::sync::watch::Receiver<u64>,
    generation: u64,
    marker: Option<&'static str>,
}

#[cfg(feature = "evaluation")]
impl HoldGate {
    pub(super) async fn wait(mut self) -> Result<()> {
        if let Some(marker) = self.marker {
            eprintln!("{marker}");
        }
        let held_at = self.generation;
        self.receiver
            .wait_for(|generation| *generation > held_at)
            .await
            .map_err(|_| anyhow::anyhow!("hold dropped with its engine"))?;
        Ok(())
    }
}

#[cfg(not(feature = "evaluation"))]
pub struct Holds;

#[cfg(not(feature = "evaluation"))]
impl Holds {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn gate<'a>(&self, failpoint: Option<&'a str>) -> (HoldPoint, Option<&'a str>) {
        (HoldPoint::None, failpoint)
    }

    pub fn release(&self) -> Result<u64> {
        anyhow::bail!("release_holds requires the evaluation feature")
    }
}

#[cfg(not(feature = "evaluation"))]
pub struct HoldGate;

#[cfg(not(feature = "evaluation"))]
impl HoldGate {
    pub(super) async fn wait(self) -> Result<()> {
        Ok(())
    }
}
