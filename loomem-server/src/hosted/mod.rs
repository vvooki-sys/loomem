//! Hosted-only adapter. Public Loomem remains single-user and knows no accounts.
mod engine;
mod protocol;
mod serve;
mod state;
#[cfg(test)]
mod test_support;

pub use engine::SharedEngine;
pub use protocol::{Request, ScopeContext};
pub use serve::{serve, ServeLimits};
