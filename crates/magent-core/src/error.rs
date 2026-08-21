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

    #[error("a change must name at least one capability, or set skip_specs")]
    MissingCapabilities,

    #[error("a change slug must be a kebab-case string")]
    InvalidChangeSlug,

    #[error("a change title must not be blank")]
    InvalidChangeTitle,

    #[error("a change rationale must not be blank")]
    InvalidChangeWhy,

    #[error("a specify command must carry at least one requirement")]
    MissingRequirements,

    #[error("a capability purpose must be at least 50 characters")]
    InvalidPurpose,

    #[error("requirement names must not repeat within one specify command")]
    DuplicateRequirementName,

    #[error("an added or modified requirement needs at least one scenario")]
    MissingScenarios,

    #[error("an added or modified requirement needs text")]
    MissingRequirementText,

    #[error("a removed requirement needs a reason")]
    MissingRemovalReason,

    #[error("a removed requirement needs a migration path")]
    MissingRemovalMigration,

    #[error("a renamed requirement needs a new name")]
    MissingRenameTarget,

    #[error("a scenario's when and then must not be blank")]
    InvalidScenario,

    #[error("a plan must carry at least one task")]
    MissingTasks,

    #[error("task numbers must not repeat within one plan")]
    DuplicateTaskNumber,

    #[error("a task number must be dot-separated digits, like 1.2 or 3.10.4")]
    InvalidTaskNumber,

    #[error("a task title must not be blank")]
    InvalidTaskTitle,

    #[error("a task's verify_command must not be blank")]
    InvalidVerifyCommand,

    #[error("a task's expected_output must name at least one marker, and no marker may be blank")]
    InvalidExpectedOutput,

    /// The output is the whole point of a tick. A task closed with no record
    /// of what its command printed is a task closed on nothing, and reads
    /// afterwards exactly like one that was checked.
    #[error("closing a task needs the output its command printed")]
    InvalidTaskOutput,

    /// A plan with a stub in its text looks finished, and falls apart on the
    /// agent that executes the one task carrying it: it sees only that task,
    /// never the surrounding plan, and has no way to guess what the stub was
    /// meant to say. Cheaper to refuse at write time than to discover it mid
    /// task.
    #[error("task {number} has {phrase:?} in its {field}, which is a stub rather than a plan")]
    PlaceholderTextInTask {
        number: String,
        field: &'static str,
        phrase: String,
    },
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
            Self::MissingCapabilities => "missing_capabilities",
            Self::InvalidChangeSlug => "invalid_change_slug",
            Self::InvalidChangeTitle => "invalid_change_title",
            Self::InvalidChangeWhy => "invalid_change_why",
            Self::MissingRequirements => "missing_requirements",
            Self::InvalidPurpose => "invalid_purpose",
            Self::DuplicateRequirementName => "duplicate_requirement_name",
            Self::MissingScenarios => "missing_scenarios",
            Self::MissingRequirementText => "missing_requirement_text",
            Self::MissingRemovalReason => "missing_removal_reason",
            Self::MissingRemovalMigration => "missing_removal_migration",
            Self::MissingRenameTarget => "missing_rename_target",
            Self::InvalidScenario => "invalid_scenario",
            Self::MissingTasks => "missing_tasks",
            Self::DuplicateTaskNumber => "duplicate_task_number",
            Self::InvalidTaskNumber => "invalid_task_number",
            Self::InvalidTaskTitle => "invalid_task_title",
            Self::InvalidVerifyCommand => "invalid_verify_command",
            Self::InvalidExpectedOutput => "invalid_expected_output",
            Self::InvalidTaskOutput => "invalid_task_output",
            Self::PlaceholderTextInTask { .. } => "placeholder_text_in_task",
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
