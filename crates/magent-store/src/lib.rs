//! Durable storage for Magent runs, checkpoints and background jobs.
//!
//! There is no daemon: hook processes and the MCP server open this store
//! directly and rely on the WAL journal plus a busy timeout to serialise
//! writes. Every mutation is idempotent on its `operation_id`, so a hook that
//! retries after a crash cannot duplicate state.

mod curation;
mod deps;
mod error;
mod facts;
mod git;
mod grouping;
mod migrations;
mod sdd;
mod sessions;
mod setup;
mod store;
mod toolchain;

pub use curation::{FactFilter, Overview, RunRow};
pub use deps::{Dependency, DependencySpec, DependencyStatus, dependency_checkout};
pub use error::StoreError;
pub use facts::{FactContext, FactQuery, namespace_candidates};
pub use git::{
    RepositoryProbe, discover, normalize_origin, state as git_state, toplevel as repository_root,
};
pub use grouping::WorkspaceGrouping;
pub use migrations::CURRENT_VERSION;
pub use sessions::SessionBinding;
pub use setup::{GroupingProposal, Sibling};
pub use store::{Job, JobState, Store, WorkspaceResolution};
pub use toolchain::detect_toolchain;
