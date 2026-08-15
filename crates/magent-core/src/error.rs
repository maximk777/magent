use thiserror::Error;

/// Every way a client-supplied command can be rejected before it reaches the
/// store. The set is closed on purpose: `code()` values are part of the wire
/// contract and are matched by the MCP layer and the hook binary.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DomainError {
    #[error("task must not be blank")]
    InvalidTask,

    #[error("at least one workspace root is required")]
    MissingWorkspaceRoot,

    #[error("handoff summary must not be blank")]
    InvalidHandoffSummary,

    #[error("a checkpoint cannot claim the completed stage; use magent_finish")]
    InvalidCheckpointStage,

    #[error("outcome must not be blank")]
    InvalidOutcome,
}

impl DomainError {
    /// Stable `snake_case` identifier for this error.
    ///
    /// Callers match on these strings, so they must never change once shipped.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidTask => "invalid_task",
            Self::MissingWorkspaceRoot => "missing_workspace_root",
            Self::InvalidHandoffSummary => "invalid_handoff_summary",
            Self::InvalidCheckpointStage => "invalid_checkpoint_stage",
            Self::InvalidOutcome => "invalid_outcome",
        }
    }
}

/// Commands that can reject themselves before any I/O happens.
pub trait Validate {
    /// # Errors
    ///
    /// Returns the first [`DomainError`] the command violates.
    fn validate(&self) -> Result<(), DomainError>;
}
