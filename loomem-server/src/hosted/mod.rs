//! Hosted-only adapter. Public Loomem remains single-user and knows no accounts.
mod engine;
mod protocol;
mod state;

pub use engine::SharedEngine;
pub use protocol::{Request, ScopeContext};
