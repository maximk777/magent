//! The spec-driven process's vocabulary.
//!
//! The graph is `OpenSpec`'s, moved from markdown into rows: a change proposes
//! deltas against capabilities, and each requirement a delta touches needs at
//! least one scenario. What this module checks is shape only — a required
//! field is present, a list is not empty, a string is not blank, names within
//! one command do not repeat. Whether a capability already exists, whether a
//! slug is taken, whether a change already has a proposal: those are
//! existence checks the store makes, not this crate, which has no way to see
//! the database.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    error::{DomainError, Validate},
    model::OperationId,
};

crate::uuid_newtype!(
    /// One proposed change moving through the spec-driven process.
    ChangeId
);

/// How much a change is expected to cost, and thus how much process it owes.
///
/// Matches `sdd_changes.classification`'s `CHECK` in
/// `0007_sdd.sql` — the string values must not drift from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// A throwaway exploration. Never reaches the specify phase.
    Spike,
    /// The common case: a change with a clear, limited scope.
    Bounded,
    /// Alters shared structure. Usually writes specs, but may legitimately
    /// skip them for a change that alters no behaviour.
    Architectural,
}

/// Where a change sits in the process.
///
/// Matches `sdd_changes.status`'s `CHECK` in `0007_sdd.sql`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Drafting,
    Specified,
    Planned,
    Executing,
    Ready,
    Archived,
    Abandoned,
}

/// What a delta does to a requirement.
///
/// Matches `spec_deltas.op`'s `CHECK` in `0007_sdd.sql`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeltaOp {
    Added,
    Modified,
    Removed,
    Renamed,
}

/// One Gherkin-shaped scenario proposed for a requirement.
///
/// `when` is not a keyword in Rust, so the field keeps the name; the SQL
/// columns are `when_text`/`then_text` because `when` and `then` are
/// reserved there. The mismatch is deliberate, and mapping between the two
/// is the store's job, not this one's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioDraft {
    pub name: String,
    #[serde(default)]
    pub given: Option<String>,
    pub when: String,
    pub then: String,
}

impl Validate for ScenarioDraft {
    fn validate(&self) -> Result<(), DomainError> {
        if self.when.trim().is_empty() || self.then.trim().is_empty() {
            return Err(DomainError::InvalidScenario);
        }
        Ok(())
    }
}

/// One requirement a change proposes to add, modify, remove or rename.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RequirementDraft {
    pub op: DeltaOp,
    pub name: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub rename_to: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub migration: Option<String>,
    /// Addresses an existing requirement by id. Required for everything but
    /// `Added`, because `Modified`, `Removed` and `Renamed` all patch a
    /// requirement that already exists rather than re-pasting its text.
    #[serde(default)]
    pub requirement_id: Option<String>,
    #[serde(default)]
    pub scenarios: Vec<ScenarioDraft>,
}

impl Validate for RequirementDraft {
    fn validate(&self) -> Result<(), DomainError> {
        match self.op {
            DeltaOp::Added | DeltaOp::Modified => {
                if self.scenarios.is_empty() {
                    return Err(DomainError::MissingScenarios);
                }
                let has_text = self
                    .text
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty());
                if !has_text {
                    return Err(DomainError::MissingRequirementText);
                }
            }
            DeltaOp::Removed => {
                if self.reason.is_none() {
                    return Err(DomainError::MissingRemovalReason);
                }
                if self.migration.is_none() {
                    return Err(DomainError::MissingRemovalMigration);
                }
            }
            DeltaOp::Renamed => {
                if self.rename_to.is_none() {
                    return Err(DomainError::MissingRenameTarget);
                }
            }
        }

        if matches!(
            self.op,
            DeltaOp::Modified | DeltaOp::Removed | DeltaOp::Renamed
        ) && self.requirement_id.is_none()
        {
            return Err(DomainError::MissingRequirementId);
        }

        for scenario in &self.scenarios {
            scenario.validate()?;
        }

        Ok(())
    }
}

/// A request to propose a change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProposeCommand {
    pub operation_id: OperationId,
    pub slug: String,
    pub title: String,
    pub classification: Classification,
    pub why: String,
    pub what_changes: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub impact: Option<String>,
    /// A change may legitimately touch no capability — a pure refactor,
    /// tooling, docs — but must say so explicitly rather than leaving
    /// `capabilities` empty by omission.
    #[serde(default)]
    pub skip_specs: bool,
}

impl Validate for ProposeCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if self.capabilities.is_empty() && !self.skip_specs {
            return Err(DomainError::MissingCapabilities);
        }
        if !is_kebab_case(&self.slug) {
            return Err(DomainError::InvalidChangeSlug);
        }
        if self.title.trim().is_empty() {
            return Err(DomainError::InvalidChangeTitle);
        }
        if self.why.trim().is_empty() {
            return Err(DomainError::InvalidChangeWhy);
        }
        Ok(())
    }
}

/// A request to attach spec deltas to a change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SpecifyCommand {
    pub operation_id: OperationId,
    pub change: ChangeId,
    pub capability_path: String,
    #[serde(default)]
    pub purpose: Option<String>,
    pub requirements: Vec<RequirementDraft>,
}

/// `OpenSpec`'s own `validate --strict` treats a purpose shorter than this as
/// too glib to be useful, and their archival step fills the gap with a
/// "TBD" placeholder nobody circles back to fill in. Rejecting the write
/// here means no placeholder is ever created to be forgotten.
const MIN_PURPOSE_LEN: usize = 50;

impl Validate for SpecifyCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if self.requirements.is_empty() {
            return Err(DomainError::MissingRequirements);
        }
        if let Some(purpose) = &self.purpose
            && purpose.len() < MIN_PURPOSE_LEN
        {
            return Err(DomainError::InvalidPurpose);
        }

        let mut seen = std::collections::HashSet::new();
        for requirement in &self.requirements {
            if !seen.insert(requirement.name.as_str()) {
                return Err(DomainError::DuplicateRequirementName);
            }
        }

        for requirement in &self.requirements {
            requirement.validate()?;
        }

        Ok(())
    }
}

/// A slug is read back as a path segment, so it is restricted to lowercase
/// ASCII letters, digits and interior hyphens.
fn is_kebab_case(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && !value.starts_with('-')
        && !value.ends_with('-')
}
