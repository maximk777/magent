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

        // Checked after the missing pieces above, so a draft with two faults
        // is told what it lacks before what it carries in excess. An addition
        // names nothing yet, so an id here is a leftover from a copied draft:
        // the store has no row to point it at and would drop it, which is the
        // one outcome this crate refuses everywhere else. The caller would go
        // on believing it had patched something while a second requirement
        // appeared beside the first.
        if matches!(self.op, DeltaOp::Added) && self.requirement_id.is_some() {
            return Err(DomainError::UnexpectedRequirementId);
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

/// One task a plan breaks a change into. The agent that executes it sees
/// only this row — never the plan around it, never a sibling task's history
/// — so anything it needs to do the work and prove it is done has to be
/// written down here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskDraft {
    /// The task's place in the plan's hierarchy — `"1"`, `"1.2"`,
    /// `"3.10.4"` — dot-separated digits with no leading or trailing dot. A
    /// run binds to its task by this number rather than by re-deriving
    /// position from list order, so it has to be stable and unambiguous on
    /// its own.
    pub number: String,
    /// One line naming the task: what an agent picking work reads first, and
    /// what a reviewer skimming the whole plan uses to place this task among
    /// the rest.
    pub title: String,
    /// The steps, code and reasoning the plan worked out for this task. The
    /// executing agent has no view of the plan that produced it, so anything
    /// it needs for "how", beyond the title, belongs here rather than being
    /// left implicit.
    #[serde(default)]
    pub body: Option<String>,
    /// Paths this task is expected to touch, named ahead of time so a
    /// reviewer can tell an expected diff from a surprising one.
    #[serde(default)]
    pub files: Vec<String>,
    /// The exact names and signatures an earlier task promised to produce,
    /// that this task now depends on. `superpowers`' idiom: the executing
    /// agent never sees the sibling task that produced them, only what is
    /// written here, so a promise not repeated here is a promise it cannot
    /// know about.
    #[serde(default)]
    pub consumes: Option<String>,
    /// The exact names and signatures this task hands to a later task in the
    /// plan. Left unset when nothing downstream depends on this task's
    /// output.
    #[serde(default)]
    pub produces: Option<String>,
    /// The exact command a caller runs to check this task actually happened.
    /// Required: a task with no way to be checked is not a task, and the
    /// executing agent has no other way to know it is done.
    pub verify_command: String,
    /// What `verify_command` should print when the task is genuinely done,
    /// so its output can be compared against something rather than eyeballed
    /// by whoever runs it.
    pub expected_output: String,
    /// Names of the requirements this task implements. Exists so "which
    /// requirement has no task covering it" is a query against this field,
    /// not a self-grade the executing agent hands itself.
    #[serde(default)]
    pub covers: Vec<String>,
}

/// Phrases borrowed from `superpowers`, where the same rule is stated in
/// prose and so goes unenforced. Checked case-insensitively against every
/// field a plan writes prose into. Extend this list rather than adding a
/// second one elsewhere.
const PLACEHOLDER_PHRASES: &[&str] = &[
    "tbd",
    "todo",
    "implement later",
    "fill in details",
    "add appropriate error handling",
    "handle edge cases",
    "similar to task",
];

/// The phrase a field carries, if any, matched on word boundaries.
///
/// Boundaries matter for the two short entries: `todo` sits inside
/// `mastodon`, and a plan for a to-do list would be refused for naming its
/// own subject. A rule that rejects valid work costs more than one that
/// misses a stub — the stub surfaces on the agent that hits it, while a false
/// refusal blocks now and offers no way around itself.
fn placeholder_in(text: &str) -> Option<&'static str> {
    let lowered = text.to_lowercase();
    let bytes = lowered.as_bytes();

    PLACEHOLDER_PHRASES.iter().copied().find(|phrase| {
        lowered.match_indices(phrase).any(|(at, matched)| {
            let before = at.checked_sub(1).map(|index| bytes[index]);
            let after = bytes.get(at + matched.len()).copied();
            let is_word = |byte: Option<u8>| byte.is_some_and(|byte| byte.is_ascii_alphanumeric());
            !is_word(before) && !is_word(after)
        })
    })
}

impl Validate for TaskDraft {
    fn validate(&self) -> Result<(), DomainError> {
        if !is_hierarchical_number(&self.number) {
            return Err(DomainError::InvalidTaskNumber);
        }
        if self.title.trim().is_empty() {
            return Err(DomainError::InvalidTaskTitle);
        }
        if self.verify_command.trim().is_empty() {
            return Err(DomainError::InvalidVerifyCommand);
        }
        if self.expected_output.trim().is_empty() {
            return Err(DomainError::InvalidExpectedOutput);
        }

        // Named field by field: the caller may have sent a dozen tasks, and
        // "one of them holds a placeholder" sends it back to re-read its own
        // input against seven phrases.
        for (field, text) in [
            ("title", Some(self.title.as_str())),
            ("body", self.body.as_deref()),
            ("verify_command", Some(self.verify_command.as_str())),
            ("expected_output", Some(self.expected_output.as_str())),
        ] {
            if let Some(phrase) = text.and_then(placeholder_in) {
                return Err(DomainError::PlaceholderTextInTask {
                    number: self.number.clone(),
                    field,
                    phrase: phrase.to_owned(),
                });
            }
        }

        Ok(())
    }
}

/// A request to break an already-specified change into an ordered list of
/// tasks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanCommand {
    pub operation_id: OperationId,
    pub change: ChangeId,
    /// The whole plan in one call. A task list is reasoned about as a unit —
    /// by the reviewer checking coverage, by the store checking every
    /// requirement has a task — and submitting it piecemeal would let a plan
    /// go stale between calls before anyone ever saw the whole of it.
    pub tasks: Vec<TaskDraft>,
}

impl Validate for PlanCommand {
    fn validate(&self) -> Result<(), DomainError> {
        if self.tasks.is_empty() {
            return Err(DomainError::MissingTasks);
        }

        let mut seen = std::collections::HashSet::new();
        for task in &self.tasks {
            if !seen.insert(task.number.as_str()) {
                return Err(DomainError::DuplicateTaskNumber);
            }
        }

        for task in &self.tasks {
            task.validate()?;
        }

        Ok(())
    }
}

/// A request to close out a change once every task on it is done.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ArchiveCommand {
    pub operation_id: OperationId,
    pub change: ChangeId,
}

impl Validate for ArchiveCommand {
    fn validate(&self) -> Result<(), DomainError> {
        // Nothing here is checkable by shape alone. Whether the change
        // exists, whether every task on it is actually done: only the store
        // knows, because only the store has the rows. Inventing a rule here
        // for the sake of symmetry with the other commands would just be a
        // check that always passes, dressed up as one that means something.
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

/// A task number is `tasks.number` in `0009_tasks.sql`: dot-separated
/// digits, hierarchical rather than sequential ("1.2", "3.10.4"), with no
/// leading, trailing or doubled dot to keep every representation of one
/// number unambiguous.
fn is_hierarchical_number(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
        })
}
