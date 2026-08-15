use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{DomainError, Validate};

/// Declares a UUID newtype that is a bare string on the wire.
///
/// The hook binary, the Web UI and `jq` in the docs all read these ids by hand,
/// so `{"0": "..."}` wrappers are not acceptable.
macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash,
            Serialize, Deserialize, JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a fresh random id.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wraps an existing UUID.
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(value)?))
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
