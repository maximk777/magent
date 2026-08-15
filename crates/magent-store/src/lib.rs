//! Durable storage for Magent runs, checkpoints and background jobs.
//!
//! There is no daemon: hook processes and the MCP server open this store
//! directly and rely on the WAL journal plus a busy timeout to serialise
//! writes. Every mutation is idempotent on its `operation_id`, so a hook that
//! retries after a crash cannot duplicate state.

mod error;
mod migrations;
mod store;

pub use error::StoreError;
pub use migrations::CURRENT_VERSION;
pub use store::{Job, Store};
