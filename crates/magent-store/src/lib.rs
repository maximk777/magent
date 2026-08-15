//! Durable storage for Magent runs, checkpoints and background jobs.
//!
//! There is no daemon: hook processes and the MCP server open this store
//! directly and rely on the WAL journal plus a busy timeout to serialise
//! writes. Every mutation is idempotent on its `operation_id`, so a hook that
//! retries after a crash cannot duplicate state.

mod error;
mod git;
mod migrations;
mod sessions;
mod store;

pub use error::StoreError;
pub use git::{
    RepositoryProbe, discover, normalize_origin, state as git_state, toplevel as repository_root,
};
pub use migrations::CURRENT_VERSION;
pub use sessions::SessionBinding;
pub use store::{Job, JobState, Store, WorkspaceResolution};
