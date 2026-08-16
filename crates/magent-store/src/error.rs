use magent_core::{ChangeId, DependencyId, DomainError, OperationId, RunId, SessionId};
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

    /// A change belongs to a workspace, and the caller's context did not name
    /// one. The column is `NOT NULL`, so the database would refuse this
    /// anyway — with a message about a constraint rather than about the
    /// context, which is where the answer actually is.
    ///
    /// The message names the fact and stops there. `resolve_workspace_for`
    /// creates a workspace on first sight of any directory, so the only way
    /// to arrive here is a resolution that failed and was discarded upstream;
    /// no tool the caller could run would change that, and telling it to run
    /// one would send it somewhere the fix is not.
    #[error("this context names no workspace to file the change under")]
    NoWorkspace,

    /// A delta was offered for a change that is not in this workspace. The
    /// foreign key on `spec_deltas.change_id` would catch a missing id, but it
    /// would say "FOREIGN KEY constraint failed" — which does not tell a
    /// caller whether it mistyped an id, or is holding one from a workspace it
    /// has since moved out of.
    #[error("change {0} does not exist in this workspace")]
    ChangeNotFound(ChangeId),

    /// Archived and abandoned changes are finished. Writing a delta onto one
    /// would edit a record of what was decided, and no constraint stops it:
    /// the status column is happy either way, so the refusal has to be here.
    #[error("change {0} is archived or abandoned and takes no further specs")]
    ChangeClosed(ChangeId),

    /// A delta naming a capability that does not exist yet is how a new
    /// capability is proposed, and a capability with no stated purpose is one
    /// nobody can review. `magent-core` checks that a purpose, if given, is
    /// long enough; only the store can see that this one is new and therefore
    /// owes one.
    #[error("capability {0:?} is new and needs a purpose")]
    CapabilityPurposeRequired(String),

    /// `OpenSpec` accepts a purpose for a capability that already has one and
    /// drops it — their own instructions admit as much. Silently discarding
    /// text a person wrote is worse than refusing it: the author believes it
    /// was recorded and never learns otherwise.
    #[error(
        "capability {0:?} already has a purpose; remove it from this delta or edit the capability"
    )]
    CapabilityPurposeRedundant(String),

    /// `Modified`, `Removed` and `Renamed` patch a requirement by id. Left to
    /// the foreign key, a requirement belonging to a *different* capability
    /// would not be caught at all — the key only knows the row exists — and a
    /// missing one would be reported as a constraint rather than as the
    /// mistyped id it is.
    ///
    /// "live" covers all three ways the target can be wrong: absent, another
    /// capability's, or already retired. A retired requirement is kept rather
    /// than deleted, so it is findable and still may not be patched — the
    /// message says *live* so that a caller holding a correct id is told what
    /// is actually the matter with it.
    #[error(
        "requirement {requirement_id} is not a live requirement of capability {capability_path:?}"
    )]
    RequirementNotFound {
        requirement_id: String,
        capability_path: String,
    },

    /// A change accumulates deltas: a second `specify` for the same capability
    /// adds to what is already proposed rather than replacing it, so a
    /// requirement name it has used once cannot be used again. The
    /// `spec_deltas_identity` index says so too, and says it as "UNIQUE
    /// constraint failed" — the message this method's other checks exist to
    /// keep a caller from having to interpret.
    #[error(
        "this change already proposes a requirement named {requirement_name:?} for capability {capability_path:?}"
    )]
    DeltaAlreadyProposed {
        requirement_name: String,
        capability_path: String,
    },
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
            Self::NoWorkspace => "no_workspace",
            Self::ChangeNotFound(_) => "change_not_found",
            Self::ChangeClosed(_) => "change_closed",
            Self::CapabilityPurposeRequired(_) => "capability_purpose_required",
            Self::CapabilityPurposeRedundant(_) => "capability_purpose_redundant",
            Self::RequirementNotFound { .. } => "requirement_not_found",
            Self::DeltaAlreadyProposed { .. } => "delta_already_proposed",
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
