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
    /// A new requirement. Needs `text` and at least one scenario; carries no
    /// `requirement_id` because there is nothing yet to point at.
    Added,
    /// Replaces an existing requirement's text. Needs `requirement_id`,
    /// `text` and at least one scenario — the whole requirement, not a
    /// diff, so nothing is lost if only part of it is supplied.
    Modified,
    /// Retires an existing requirement. Needs `requirement_id`, `reason`
    /// and `migration`: a removal without a path forward is a break nobody
    /// explained.
    Removed,
    /// Renames an existing requirement without changing its text. Needs
    /// `requirement_id` and `rename_to`.
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
    /// The requirement's full text. Required, and must be non-blank, for
    /// `Added` and `Modified`; left `None` for `Removed` and `Renamed`,
    /// which do not change what the requirement says.
    #[serde(default)]
    pub text: Option<String>,
    /// The requirement's new name. Required for `Renamed`, meaningless for
    /// every other op.
    #[serde(default)]
    pub rename_to: Option<String>,
    /// Why the requirement is going away. Required for `Removed`.
    #[serde(default)]
    pub reason: Option<String>,
    /// What a caller relying on the removed requirement should do instead.
    /// Required for `Removed`, alongside `reason`: a removal without a path
    /// forward is a break nobody explained.
    #[serde(default)]
    pub migration: Option<String>,
    /// Addresses an existing requirement by id. Required for everything but
    /// `Added`, because `Modified`, `Removed` and `Renamed` all patch a
    /// requirement that already exists rather than re-pasting its text.
    #[serde(default)]
    pub requirement_id: Option<String>,
    /// Required, and must be non-empty, for `Added` and `Modified`: a
    /// requirement with no scenario can only be asserted, never checked
    /// against. `OpenSpec` states the same rule in prose, where a scenario
    /// written with the wrong heading simply is not read as one and the
    /// requirement loses it without complaint.
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
    /// The change's address: lowercase letters, digits and single interior
    /// hyphens, free while the change is in flight and released again once
    /// it is archived or abandoned.
    pub slug: String,
    /// One line, readable on its own in a list of open changes.
    pub title: String,
    pub classification: Classification,
    /// Why this change is worth making. Not a summary of what changes —
    /// `what_changes` covers that — but the reasoning a reviewer needs to
    /// judge whether it should happen at all.
    pub why: String,
    /// The change in outline, one entry per notable edit. What a reviewer
    /// reads before the diff.
    pub what_changes: Vec<String>,
    /// Paths of the capabilities this change touches, `worker/retry`-style.
    /// A path need not exist yet: naming a new one is how a change proposes
    /// to add it, and the deltas that spell out what changes come from a
    /// later `magent_specify` call, not from here.
    pub capabilities: Vec<String>,
    /// What could go wrong, or who else is affected. Free text, and
    /// optional: not every change carries a risk worth naming.
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
    /// The capability these requirements belong to, `worker/retry`-style.
    /// Must match one of the paths the change's `magent_propose` call
    /// named.
    pub capability_path: String,
    /// What the capability is for, in prose. Only meaningful when the
    /// capability is new — an existing one already has a purpose on
    /// record — but whenever it is given, it must run at least 50
    /// characters: `OpenSpec`'s own strict validator uses the same floor,
    /// because anything shorter reads as a placeholder rather than an
    /// explanation.
    #[serde(default)]
    pub purpose: Option<String>,
    pub requirements: Vec<RequirementDraft>,
}

/// `OpenSpec`'s own `validate --strict` treats a purpose shorter than this as
/// too glib to be useful, and their archival step fills the gap with a
/// "TBD" placeholder nobody circles back to fill in. Rejecting the write
/// here means no placeholder is ever created to be forgotten.
///
/// Counted in characters, not bytes: `String::len()` would halve the
/// effective floor for any non-ASCII writer, Cyrillic included, since this
/// project is worked in both English and Russian.
const MIN_PURPOSE_LEN: usize = 50;

impl Validate for SpecifyCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if self.requirements.is_empty() {
            return Err(DomainError::MissingRequirements);
        }
        if let Some(purpose) = &self.purpose
            && purpose.chars().count() < MIN_PURPOSE_LEN
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
/// ASCII letters, digits and single interior hyphens. Matches `is_slug` in
/// `fact.rs`, for the same reason: an address has to survive a filename and
/// a URL unchanged, and `"add--retry"` is not obviously distinct from
/// `"add-retry"` once it has.
fn is_kebab_case(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}
