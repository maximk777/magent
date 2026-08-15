//! The memory layer's vocabulary.
//!
//! A fact carries four things a note does not: where it applies, how it may
//! conflict with another fact, how much it should be trusted, and what it was
//! learned from. Those are what make memory queryable rather than merely
//! stored — and what let a contradiction be recorded instead of silently
//! overwriting what was there.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    error::{DomainError, Validate},
    model::OperationId,
};

crate::uuid_newtype!(
    /// One durable fact.
    FactId
);

/// Where a fact applies.
///
/// Ordered from general to specific so a repository-level fact can outrank a
/// user-level one about the same subject.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FactScope {
    /// About the person: preferences, background, how they like to work.
    User,
    /// About a group of repositories worked on together.
    Workspace,
    /// About one codebase.
    Repository,
    /// About one task, and rarely worth keeping past it.
    Run,
}

/// What kind of thing a fact is.
///
/// These are the four categories the existing corpus already uses, so imported
/// memory keeps its meaning instead of being flattened into "notes".
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// Who the user is and what they prefer.
    User,
    /// Guidance the user gave about how to work. Carries a reason.
    Feedback,
    /// State of ongoing work that the code and git history do not record.
    Project,
    /// A pointer to something external.
    Reference,
}

/// How two values of the same subject may coexist.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// One current value per scope. A new one supersedes the old, which is kept
    /// as a revision rather than deleted.
    Single,
    /// Several distinct values coexist.
    Set,
    /// Values hold over intervals, and conflict only where those overlap.
    Timeline,
}

impl Cardinality {
    /// Whether two values in one scope can contradict each other.
    ///
    /// `Set` values never do, so they are stored side by side without a
    /// supersede check.
    #[must_use]
    pub const fn conflicts_within_scope(self) -> bool {
        match self {
            Self::Single | Self::Timeline => true,
            Self::Set => false,
        }
    }
}

/// How much a fact should be trusted, and whether it still holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FactStatus {
    /// Seen directly, but not checked.
    Observed,
    /// Concluded rather than seen. The weakest claim.
    Inferred,
    /// Checked against evidence.
    Verified,
    /// A later fact disagrees. Kept, because knowing two sources disagree is
    /// itself worth knowing.
    Contradicted,
    /// Probably out of date.
    Stale,
    /// Withdrawn deliberately.
    Revoked,
}

/// How one fact relates to another.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// Mentioned together, no stronger claim. What a wikilink means.
    Related,
    /// Replaces an earlier fact.
    Supersedes,
    /// Disagrees with another fact.
    Contradicts,
    /// Narrows or specialises another fact.
    Refines,
}

/// Where a fact came from.
///
/// The locator is deliberately not the content: a path and line, a commit, a
/// URL. Copying the source in would duplicate it and risk carrying secrets into
/// memory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    pub locator: String,
    /// A short quotation, when one makes the locator meaningful on its own.
    #[serde(default)]
    pub excerpt: Option<String>,
}

/// A request to remember something.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RememberCommand {
    pub operation_id: OperationId,
    /// Stable slug, unique within its scope. This is the address other facts
    /// supersede or link to.
    pub name: String,
    /// One line, readable on its own in an index.
    pub title: String,
    /// The statement, with its reasoning.
    pub body: String,
    pub kind: FactKind,
    pub scope: FactScope,
    pub cardinality: Cardinality,
    pub status: FactStatus,
    /// 0.0 to 1.0.
    pub confidence: f64,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// Names of other facts this one relates to. The target need not exist yet:
    /// a link to a fact not written down is a note about what is missing.
    #[serde(default)]
    pub relates_to: Vec<(String, RelationKind)>,
}

impl Validate for RememberCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if !is_slug(&self.name) {
            return Err(DomainError::InvalidFactName);
        }
        if self.title.trim().is_empty() && self.body.trim().is_empty() {
            return Err(DomainError::InvalidFactBody);
        }
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(DomainError::InvalidConfidence);
        }
        if self.status == FactStatus::Verified && self.evidence.is_empty() {
            return Err(DomainError::VerifiedWithoutEvidence);
        }
        Ok(())
    }
}

/// A name has to survive a filename, a wikilink and a URL unchanged, so it is
/// restricted to lowercase, digits and single interior hyphens.
fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 120
        && value.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}

/// A stored fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Fact {
    pub fact_id: FactId,
    pub name: String,
    pub title: String,
    pub body: String,
    pub kind: FactKind,
    pub scope: FactScope,
    pub cardinality: Cardinality,
    pub status: FactStatus,
    pub confidence: f64,
    /// Set for imported facts whose workspace is not yet known.
    #[serde(default)]
    pub namespace: Option<String>,
    pub evidence: Vec<Evidence>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// One line of the index pushed into context.
///
/// Deliberately without the body: the point is to tell the model what exists so
/// it can ask for what it needs, at a fraction of the tokens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FactSummary {
    pub fact_id: FactId,
    pub name: String,
    pub title: String,
    pub kind: FactKind,
    pub scope: FactScope,
    pub status: FactStatus,
}
