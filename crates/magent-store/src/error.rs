use magent_core::{DependencyId, DomainError, OperationId, RunId, SessionId};
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Domain(#[from] DomainError),

    #[error("database error: {0}")]
    Database(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("run {0} does not exist")]
    RunNotFound(RunId),

    #[error("session {0} does not exist")]
    SessionNotFound(SessionId),

    #[error("run {0} is already completed")]
    RunClosed(RunId),

    /// The same `operation_id` arrived carrying a different request. Replaying
    /// the stored response would answer a question that was never asked.
    #[error("operation {0} was already used for a different request")]
    IdempotencyConflict(OperationId),

    #[error("database schema version {0} is newer than this build understands")]
    UnsupportedSchema(i64),

    /// A tool that works on "the run in flight" was called with nothing in
    /// flight. Opening one here would produce a run with no task.
    #[error("no run is open in this workspace; call magent_start first")]
    NoOpenRun,

    #[error("dependency {0} does not exist")]
    DependencyNotFound(DependencyId),

    /// The unique index on live slugs (`sdd_changes_live_slug`) would catch
    /// this too, but "UNIQUE constraint failed" does not tell a caller what
    /// to do about it. Checked explicitly so the message does.
    #[error("slug {0:?} is already in use by a change in flight")]
    SlugTaken(String),
}

impl StoreError {
    /// Stable `snake_case` identifier, surfaced to the model through MCP.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Domain(inner) => inner.code(),
            Self::Database(_) => "database_error",
            Self::Serialization(_) => "serialization_error",
            Self::RunNotFound(_) => "run_not_found",
            Self::SessionNotFound(_) => "session_not_found",
            Self::RunClosed(_) => "run_closed",
            Self::IdempotencyConflict(_) => "idempotency_conflict",
            Self::UnsupportedSchema(_) => "unsupported_schema",
            Self::NoOpenRun => "no_open_run",
            Self::DependencyNotFound(_) => "dependency_not_found",
            Self::SlugTaken(_) => "slug_taken",
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error.to_string())
    }
}
