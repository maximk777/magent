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

    #[error("a fact name must be a lowercase slug")]
    InvalidFactName,

    #[error("a fact must have a title or a body")]
    InvalidFactBody,

    #[error("confidence must be between 0 and 1")]
    InvalidConfidence,

    #[error("a verified fact must cite evidence")]
    VerifiedWithoutEvidence,
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
            Self::InvalidFactName => "invalid_fact_name",
            Self::InvalidFactBody => "invalid_fact_body",
            Self::InvalidConfidence => "invalid_confidence",
            Self::VerifiedWithoutEvidence => "verified_without_evidence",
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
