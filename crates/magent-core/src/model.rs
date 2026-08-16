use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{DomainError, Validate};

/// Declares a UUID newtype that is a bare string on the wire.
///
/// The hook binary, the Web UI and `jq` in the docs all read these ids by hand,
/// so `{"0": "..."}` wrappers are not acceptable.
#[macro_export]
macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash,
            ::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(::uuid::Uuid);

        impl $name {
            /// Generates a fresh random id.
            #[must_use]
            pub fn new() -> Self {
                Self(::uuid::Uuid::new_v4())
            }

            /// Wraps an existing UUID.
            #[must_use]
            pub const fn from_uuid(uuid: ::uuid::Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> ::uuid::Uuid {
                self.0
            }
        }

        impl ::core::default::Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, formatter)
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = ::uuid::Error;

            fn from_str(value: &str) -> ::core::result::Result<Self, Self::Err> {
                Ok(Self(::uuid::Uuid::parse_str(value)?))
            }
        }
    };
}

uuid_newtype!(
    /// One user-facing task. Outlives any single session or harness.
    RunId
);
uuid_newtype!(
    /// One harness session participating in a run.
    SessionId
);
uuid_newtype!(
    /// One durable checkpoint within a run.
    CheckpointId
);
uuid_newtype!(
    /// Idempotency key for a single mutating operation.
    OperationId
);
uuid_newtype!(
    /// A group of repositories worked on together.
    WorkspaceId
);
uuid_newtype!(
    /// A single git repository, identified by canonical path and origin.
    RepositoryId
);
uuid_newtype!(
    /// A reference checkout: a repository the workspace reads but does not
    /// work in.
    DependencyId
);

/// Where a run currently sits in its workflow.
///
/// `Completed` is reachable only through `magent_finish`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStage {
    Discovering,
    Planning,
    Executing,
    Verifying,
    Reviewing,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Open,
    Completed,
}

/// How a checkpoint was produced.
///
/// `Deterministic` ones are written synchronously by the `PreCompact` hook and
/// contain only observable facts. `Enriched` ones have been through the
/// background distiller and additionally carry reasoning.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointOrigin {
    Deterministic,
    Enriched,
}

/// Closing a session hands work over. Completing the run ends it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FinishAction {
    CloseSession,
    CompleteRun,
}

/// Which harness a session belongs to. Resolved by the server from how it was
/// launched, never supplied by the client.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    ClaudeCode,
    Codex,
    OpenCode,
    Unknown,
}

/// How freely a repository may be touched.
///
/// Recorded rather than inferred: an agent cannot tell a service it was asked
/// to change from the infrastructure that deploys a dozen of them by looking at
/// the code, and the cost of guessing wrong is not symmetric.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryRole {
    /// The thing being worked on.
    #[default]
    Primary,
    /// Context. Changed only by an explicit decision.
    Related,
    /// Never changed.
    ReadOnly,
    /// Changing it affects everything else in the group.
    Infrastructure,
}

/// Git state captured at a point in time, for context and handoff only.
///
/// Deliberately excludes the origin URL: that identifies the repository and
/// does not change moment to moment, so keeping it here would force an extra
/// subprocess on the `PreCompact` path, which has a 100 ms budget.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GitState {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Full object id, so it can be compared exactly.
    pub sha: Option<String>,
    /// Files with uncommitted changes, untracked ones included. Magent records
    /// this and never cleans it.
    pub dirty_files: u32,
}

/// One observed mutation during a run, recorded by the `PostToolUse` hook.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileLedgerEntry {
    pub path: PathBuf,
    pub tool: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StartRunCommand {
    pub operation_id: OperationId,
    pub task: String,
    /// Set to continue an existing run from a different session or harness.
    pub resume_run_id: Option<RunId>,
    pub external_session_hint: Option<String>,
    pub workspace_roots: Vec<PathBuf>,
}

impl Validate for StartRunCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if self.task.trim().is_empty() {
            return Err(DomainError::InvalidTask);
        }
        if self.workspace_roots.is_empty() {
            return Err(DomainError::MissingWorkspaceRoot);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointCommand {
    pub operation_id: OperationId,
    pub run_id: RunId,
    pub session_id: SessionId,
    pub stage: WorkflowStage,
    pub origin: CheckpointOrigin,
    pub completed_steps: Vec<String>,
    pub next_steps: Vec<String>,
    pub decisions: Vec<String>,
    /// Alternatives considered and turned down. Without these a later session
    /// re-litigates settled questions.
    pub rejected: Vec<String>,
    pub changed_files: Vec<String>,
    pub verification: Vec<String>,
    pub risks: Vec<String>,
    pub handoff_summary: String,
}

impl Validate for CheckpointCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if self.handoff_summary.trim().is_empty() {
            return Err(DomainError::InvalidHandoffSummary);
        }
        if self.stage == WorkflowStage::Completed {
            return Err(DomainError::InvalidCheckpointStage);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FinishRunCommand {
    pub operation_id: OperationId,
    pub run_id: RunId,
    pub session_id: SessionId,
    pub action: FinishAction,
    pub outcome: String,
}

impl Validate for FinishRunCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if self.outcome.trim().is_empty() {
            return Err(DomainError::InvalidOutcome);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointSnapshot {
    pub checkpoint_id: CheckpointId,
    pub run_id: RunId,
    pub session_id: SessionId,
    pub stage: WorkflowStage,
    pub origin: CheckpointOrigin,
    pub completed_steps: Vec<String>,
    pub next_steps: Vec<String>,
    pub decisions: Vec<String>,
    pub rejected: Vec<String>,
    pub changed_files: Vec<String>,
    pub verification: Vec<String>,
    pub risks: Vec<String>,
    pub handoff_summary: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StartRunResult {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub task: String,
    pub stage: WorkflowStage,
    pub latest_checkpoint: Option<CheckpointSnapshot>,
    pub instructions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointResult {
    pub checkpoint_id: CheckpointId,
    pub run_id: RunId,
    pub stage: WorkflowStage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FinishRunResult {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub status: RunStatus,
    pub session_closed: bool,
    pub outcome: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunSnapshot {
    pub run_id: RunId,
    pub workspace_id: WorkspaceId,
    pub task: String,
    pub status: RunStatus,
    pub stage: WorkflowStage,
    pub latest_checkpoint: Option<CheckpointSnapshot>,
    /// The spec change this run is executing, when it is executing one. `None`
    /// for the plenty of work that is not spec-driven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<SpecBinding>,
}

/// A reference into `openspec/`, never a copy of it.
///
/// The proposal and the task list are files in git and they are the source of
/// truth. What they cannot say is which task is in flight in this session,
/// after a compaction that erased the model's own memory of it — that is what
/// this holds, and it holds nothing else. Copying the spec here would create a
/// second version that drifts, and the moment the two disagree the wrong one is
/// the one the agent trusts.
///
/// Every field is optional on the way in, where `None` means "leave as it is".
/// Advancing to the next task must not require restating the change; making the
/// caller repeat it is how a run ends up half-bound.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SpecBinding {
    /// The change's directory name, `add-retry-budget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Repository-relative paths to the proposal and task list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// The task in flight, as it reads in the list: `2: wire the budget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task: Option<String>,
}

/// A repository as Magent identifies it: canonical path plus origin URL.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Repository {
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub canonical_root: PathBuf,
    pub origin_url: Option<String>,
}

impl Repository {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }
}
