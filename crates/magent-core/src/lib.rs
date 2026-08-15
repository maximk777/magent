//! Domain contracts shared by every Magent surface.
//!
//! This crate is deliberately I/O free: it holds the command and snapshot
//! shapes plus their validation rules, so the store, the MCP server, the hook
//! binary and the Web UI all agree on one vocabulary.

mod error;
mod fact;
mod model;

pub use error::{DomainError, Validate};
pub use fact::{
    Cardinality, Evidence, Fact, FactId, FactKind, FactScope, FactStatus, FactSummary,
    RelationKind, RememberCommand,
};
pub use model::{
    CheckpointCommand, CheckpointId, CheckpointOrigin, CheckpointResult, CheckpointSnapshot,
    FileLedgerEntry, FinishAction, FinishRunCommand, FinishRunResult, GitState, HarnessKind,
    OperationId, Repository, RepositoryId, RepositoryRole, RunId, RunSnapshot, RunStatus,
    SessionId, StartRunCommand, StartRunResult, WorkflowStage, WorkspaceId,
};
