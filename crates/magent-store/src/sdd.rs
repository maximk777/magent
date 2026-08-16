//! The spec-driven process, as rows.
//!
//! `magent-core` knows the shape a proposal must take but nothing about what
//! is already in the database, so the checks that depend on existing rows —
//! is this slug already live — belong here rather than there.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use magent_core::{
    ArchiveCommand, ChangeId, ChangeStatus, Classification, DeltaOp, PlanCommand, ProposeCommand,
    RequirementDraft, SpecifyCommand, Validate,
};
use rusqlite::{OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::{
    error::StoreError,
    facts::FactContext,
    store::{Store, enum_from_sql, enum_to_sql, parse_id, parse_timestamp},
};

/// What `sdd_artifacts.body_json` holds for a `kind = 'proposal'` row.
///
/// A plain projection of the parts of [`ProposeCommand`] a proposal
/// document needs to show — everything except the identity and process
/// fields (`operation_id`, `slug`, `title`, `classification`, `skip_specs`),
/// which already have columns of their own on `sdd_changes`.
#[derive(Serialize)]
struct ProposalBody<'a> {
    why: &'a str,
    what_changes: &'a [String],
    capabilities: &'a [String],
    impact: Option<&'a str>,
}

/// The one field of a proposal `specify` reads back: the capabilities the
/// change declared it would touch.
///
/// A reading counterpart to [`ProposalBody`] rather than a `Deserialize` on
/// that type, which borrows from the string it was parsed out of and so cannot
/// outlive the query. Every other field is ignored on purpose: what this
/// answers is one question, and a struct that had to name the whole document
/// would need changing every time the document grew a field.
#[derive(Deserialize)]
struct DeclaredCapabilities {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// What a `propose` did, in terms the caller can check against what it sent.
///
/// The identifier alone — what this used to return — cannot tell a change that
/// was opened from one that was rewritten, and rewriting is the ordinary way to
/// widen a change's scope (see [`Store::propose`]). `status` matters for the
/// same reason: a rewrite that moves the capability list sends the change back
/// to `drafting`, and a caller that believed it was still `specified` would
/// plan against a spec the store no longer accepts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProposeReport {
    pub id: ChangeId,
    /// Echoed back so a caller addressing changes by slug — which is what the
    /// MCP layer lets it do — can see the one it now holds.
    pub slug: String,
    /// Where the change sits after this call: `drafting` for a new one, and
    /// for a rewrite whatever it kept or was sent back to.
    pub status: ChangeStatus,
    /// Set when this call rewrote a proposal that already stood under this
    /// slug rather than opening a new change.
    pub rewritten: bool,
}

/// What a `specify` wrote, in terms the caller can check against what it sent.
///
/// This is read by a model through MCP, so it names the capability it filed
/// the deltas under and counts them by operation rather than returning ids:
/// "two added, one removed, now specified" is something a caller can compare
/// with the command it wrote, and a list of uuids is not.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpecifyReport {
    pub capability_path: String,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub renamed: usize,
    /// Where the change now sits — `specified`, since that is what this call
    /// moves it to.
    pub status: ChangeStatus,
}

/// What a `plan` wrote, in terms the caller can check against what it sent.
///
/// A count of the whole plan rather than of what this call added, because a
/// plan replaces whatever stood before it: "four tasks, now planned" is the
/// state of the change, and after a re-plan of two tasks it says two.
///
/// There is deliberately no list of uncovered requirements here. A plan that
/// leaves one uncovered is refused outright
/// ([`StoreError::RequirementsUncovered`]), so the field could only ever come
/// back empty — and a field that is always empty reads as a promise the caller
/// is meant to check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanReport {
    pub tasks: usize,
    /// Where the change now sits — `planned`, since that is what this call
    /// moves it to.
    pub status: ChangeStatus,
}

/// What an `archive` folded into the live base, in terms the caller can check
/// against the deltas it wrote.
///
/// Counted by operation like [`SpecifyReport`], because that is the shape the
/// caller can compare with the change it has been reading all along. The
/// capabilities are named rather than counted: a capability created here is a
/// new heading in the live base that existed nowhere before this call, and
/// "one capability created" leaves the caller to work out which.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveReport {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub renamed: usize,
    /// Paths of the capabilities this call created, in the order the deltas
    /// were applied.
    pub capabilities_created: Vec<String>,
    /// Where the change now sits — `archived`, since that is what this call
    /// moves it to.
    pub status: ChangeStatus,
}

/// One change as [`Store::open_changes`] lists it: enough to answer "where
/// did I leave off" without a query per row.
///
/// The counts are read here rather than left for a second call, because that
/// is exactly the query a caller who only has a list of ids would otherwise
/// have to run once per change — the loop this module refuses to write
/// (see `load_deltas`, `require_full_coverage`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub id: ChangeId,
    pub slug: String,
    pub title: String,
    pub classification: Classification,
    pub status: ChangeStatus,
    /// When the change, or anything filed under it, was last touched. What
    /// [`Store::open_changes`] orders by, freshest first.
    pub updated_at: DateTime<Utc>,
    /// How many deltas this change's `specify` calls have written so far.
    pub delta_count: usize,
    /// How many tasks its current plan carries — zero before it has one.
    pub task_count: usize,
    /// How many of those tasks are `done` or `skipped`. Read against
    /// `task_count`, this is what says whether execution has finished.
    pub tasks_closed: usize,
}

/// One delta as [`Store::change_detail`] shows it.
///
/// Only what a caller re-reading its own change needs to see where things
/// stand: `requirement_id`, `text`, `reason`, `migration` and the scenarios
/// stay in the database, because a caller asking this question already wrote
/// them and is not asking for them back.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeltaSummary {
    pub op: DeltaOp,
    pub name: String,
    pub capability_path: String,
}

/// One task as [`Store::change_detail`] shows it.
///
/// `body`, `consumes`/`produces` and the evidence a finished task carries stay
/// in the database: those are what an agent executing *that* task reads from
/// its own row, not what a caller asking "where did this change get to" needs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub number: String,
    pub title: String,
    /// `pending`, `running`, `done` or `skipped` — `tasks.status`'s own
    /// values (`0009_tasks.sql`), passed through rather than re-typed as an
    /// enum `magent-core` does not define for tasks.
    pub status: String,
    pub verify_command: String,
}

/// The full content of one change: [`ChangeSummary`]'s fields, the proposal's
/// own words, and the deltas and tasks filed under it.
///
/// This is what closes the gap `open_changes` cannot: a caller that has lost
/// everything but a `change_id` — the ordinary state after a context
/// compaction — reads this once and has the proposal, the specs and the plan
/// back, in the terms it would have written them in.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeDetail {
    pub id: ChangeId,
    pub slug: String,
    pub title: String,
    pub classification: Classification,
    pub status: ChangeStatus,
    pub updated_at: DateTime<Utc>,
    pub delta_count: usize,
    pub task_count: usize,
    pub tasks_closed: usize,
    /// Why this change is being made, in the proposal's own words.
    pub why: String,
    pub what_changes: Vec<String>,
    /// The capabilities the proposal declared — the contract every `specify`
    /// call on this change was checked against.
    pub capabilities: Vec<String>,
    pub impact: Option<String>,
    /// Every delta this change has specified, in the order [`Store::archive`]
    /// would apply them.
    pub deltas: Vec<DeltaSummary>,
    /// Every task of its current plan, in task-number order.
    pub tasks: Vec<TaskSummary>,
}

impl Store {
    /// Opens a change: one row in `sdd_changes` with status `drafting`, and
    /// one row in `sdd_artifacts` holding the proposal, written together.
    ///
    /// Called again for a slug held by a change still `drafting` or
    /// `specified`, it *rewrites* that change's proposal rather than refusing:
    /// title, classification, `skip_specs` and the proposal document are
    /// replaced in place, and the change keeps its id and its status. An
    /// author who reaches the specify phase and finds a capability missing
    /// from the proposal has to be able to declare it — [`Store::specify`]
    /// accepts only capabilities the proposal names, so without the rewrite
    /// that author has no way forward at all. `UNIQUE(change_id, kind)` on
    /// `sdd_artifacts` is what makes the proposal a single row that a rewrite
    /// overwrites; `0007_sdd.sql` explains why no revision history is kept.
    /// Which of the two happened is on the [`ProposeReport`] this returns —
    /// the caller cannot see it from the id, which is the same either way.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Domain`] for an invalid command,
    /// [`StoreError::NoWorkspace`] when the context names no workspace to
    /// file the change under, [`StoreError::SlugTaken`] when the slug names a
    /// change already past its proposal,
    /// [`StoreError::CapabilityDeltasStranded`] when the rewrite drops a
    /// capability whose deltas are already written, or a database error.
    pub fn propose(
        &self,
        command: &ProposeCommand,
        context: &FactContext,
    ) -> Result<ProposeReport, StoreError> {
        command.validate()?;

        // Resolved before the writer lock, for the same reason validation is:
        // the column is NOT NULL, so the database would refuse this anyway,
        // but it would refuse it by naming a constraint. What the caller has
        // to fix is the working directory, and only this layer knows that.
        let workspace_id = context.workspace_id.ok_or(StoreError::NoWorkspace)?;

        self.execute_operation("propose", command.operation_id, command, |tx| {
            // Mirrors sdd_changes_live_slug (0007_sdd.sql): a slug is held
            // only by a change that has not yet been archived or abandoned.
            // Relying on the unique index itself to report a collision would
            // surface a raw "UNIQUE constraint failed" to the caller, which
            // does not say what to do about it — and would refuse the rewrite
            // below outright.
            let live: Option<(String, String)> = tx
                .query_row(
                    "SELECT id, status FROM sdd_changes
                     WHERE workspace_id = ?1 AND namespace IS ?2 AND slug = ?3
                       AND status NOT IN ('archived', 'abandoned')",
                    rusqlite::params![
                        workspace_id.to_string(),
                        context.namespace.as_deref(),
                        &command.slug,
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;

            let now = Utc::now().to_rfc3339();

            if let Some((existing_id, status)) = live {
                return rewrite_proposal(tx, &existing_id, enum_from_sql(&status)?, command, &now);
            }

            let change_id = ChangeId::new();

            tx.execute(
                "INSERT INTO sdd_changes (
                     id, workspace_id, namespace, slug, title, classification,
                     skip_specs, status, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                rusqlite::params![
                    change_id.to_string(),
                    workspace_id.to_string(),
                    context.namespace.as_deref(),
                    &command.slug,
                    &command.title,
                    enum_to_sql(&command.classification)?,
                    command.skip_specs,
                    enum_to_sql(&ChangeStatus::Drafting)?,
                    &now,
                ],
            )?;
            write_proposal(tx, &change_id.to_string(), command, &now)?;

            Ok(ProposeReport {
                id: change_id,
                slug: command.slug.clone(),
                status: ChangeStatus::Drafting,
                rewritten: false,
            })
        })
    }

    /// Attaches a capability's deltas to a change and moves it to
    /// `specified`.
    ///
    /// Every delta and every scenario lands in one transaction: a spec is one
    /// artifact, and half of it is not a smaller spec but an unreviewed one.
    ///
    /// The capability has to be one the change's proposal declared —
    /// `OpenSpec` calls that list the contract between the proposal and the
    /// specs, and it is what keeps a spec from quietly widening the scope that
    /// was agreed. A proposal that named the wrong capability is corrected by
    /// calling [`Store::propose`] again, which rewrites it.
    ///
    /// Called again for the same capability, it *adds* to the deltas the
    /// change already carries — nothing written earlier is replaced or
    /// removed, and a requirement name used once cannot be used again. There
    /// is deliberately no way to edit a delta here: correcting one is
    /// reworking the change, not appending to it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Domain`] for an invalid command,
    /// [`StoreError::NoWorkspace`] when the context names no workspace,
    /// [`StoreError::ChangeNotFound`] or [`StoreError::ChangeClosed`] when the
    /// change cannot take deltas, [`StoreError::CapabilityNotProposed`] when
    /// the capability is not one the proposal declared,
    /// [`StoreError::CapabilityPurposeRequired`] or
    /// [`StoreError::CapabilityPurposeRedundant`] when the purpose does not
    /// match what the capability needs, [`StoreError::RequirementNotFound`]
    /// when a delta patches a requirement this capability does not have live,
    /// [`StoreError::DeltaAlreadyProposed`] when it repeats a requirement name
    /// this change has already used, or a database error.
    pub fn specify(
        &self,
        command: &SpecifyCommand,
        context: &FactContext,
    ) -> Result<SpecifyReport, StoreError> {
        command.validate()?;

        // Outside the closure for the same reason `Store::propose` resolves it
        // outside its own: taking the writer lock to discover something
        // already known costs another writer its turn. Named rather than
        // pointed at, because a third verb landing between them would make
        // "above" quietly wrong and no compiler would say so.
        let workspace_id = context.workspace_id.ok_or(StoreError::NoWorkspace)?;

        self.execute_operation("specify", command.operation_id, command, |tx| {
            let change_id = command.change.to_string();
            let workspace = workspace_id.to_string();
            let namespace = require_open_change(tx, command.change, &workspace)?;
            // Ahead of `resolve_capability`, so that a capability nobody
            // proposed is answered as such rather than as one owing a purpose:
            // the first is a disagreement about scope, the second a detail of
            // a capability this change was never going to touch.
            require_proposed_capability(tx, &change_id, command)?;
            let capability_id = resolve_capability(tx, &workspace, namespace.as_deref(), command)?;

            let now = Utc::now().to_rfc3339();
            let mut report = SpecifyReport {
                capability_path: command.capability_path.clone(),
                added: 0,
                modified: 0,
                removed: 0,
                renamed: 0,
                status: ChangeStatus::Specified,
            };

            for requirement in &command.requirements {
                let requirement_id =
                    resolve_requirement(tx, capability_id.as_deref(), requirement, command)?;
                require_unproposed(tx, &change_id, command, requirement)?;
                write_delta(
                    tx,
                    &change_id,
                    capability_id.as_deref(),
                    command,
                    requirement,
                    requirement_id,
                    &now,
                )?;

                match requirement.op {
                    DeltaOp::Added => report.added += 1,
                    DeltaOp::Modified => report.modified += 1,
                    DeltaOp::Removed => report.removed += 1,
                    DeltaOp::Renamed => report.renamed += 1,
                }
            }

            // From any open status, not only from `drafting`: a change that
            // was already `planned` comes *back* to `specified`. The plan was
            // written against the spec as it stood, so a spec that has moved
            // leaves it describing work nobody agreed to. Better the next step
            // finds a change waiting to be re-planned than one sitting at
            // `planned` under a plan that no longer covers it.
            tx.execute(
                "UPDATE sdd_changes SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![enum_to_sql(&ChangeStatus::Specified)?, &now, &change_id,],
            )?;

            Ok(report)
        })
    }

    /// Writes a change's task list and moves it to `planned`.
    ///
    /// The whole plan lands in one transaction, and — unlike [`Store::specify`],
    /// which accumulates deltas — a second call *replaces* the tasks the change
    /// already had rather than adding to them. A plan is one connected thing:
    /// its numbers and the dependencies its tasks declare on each other are
    /// agreed against one another, so a task appended to the side of one
    /// belongs to a plan nobody reviewed. Re-planning submits the whole list
    /// again, and what was there before goes.
    ///
    /// Task numbers are unique per change (`tasks_number`), and nothing here
    /// checks that separately: duplicates within one command are
    /// `magent-core`'s to catch, and the tasks already on the change are gone
    /// by the time these are written.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Domain`] for an invalid command,
    /// [`StoreError::NoWorkspace`] when the context names no workspace,
    /// [`StoreError::ChangeNotFound`] or [`StoreError::ChangeClosed`] when the
    /// change cannot be planned at all, [`StoreError::ChangeNotSpecified`]
    /// when it has no spec to plan against,
    /// [`StoreError::RequirementsUncovered`] when a requirement the change
    /// proposes has no task implementing it, or a database error.
    pub fn plan(
        &self,
        command: &PlanCommand,
        context: &FactContext,
    ) -> Result<PlanReport, StoreError> {
        command.validate()?;

        // Outside the closure, as in `Store::propose` and `Store::specify`:
        // an answer already known is not worth another writer's turn at the
        // lock.
        let workspace_id = context.workspace_id.ok_or(StoreError::NoWorkspace)?;

        self.execute_operation("plan", command.operation_id, command, |tx| {
            let change_id = command.change.to_string();
            require_plannable_change(tx, command.change, &workspace_id.to_string())?;
            require_full_coverage(tx, &change_id, command)?;

            let now = Utc::now().to_rfc3339();

            // The replacement the doc comment describes. Ahead of the inserts
            // rather than after them, so a re-plan reusing a number is not
            // fighting `tasks_number` with itself.
            tx.execute("DELETE FROM tasks WHERE change_id = ?1", [&change_id])?;

            for task in &command.tasks {
                tx.execute(
                    "INSERT INTO tasks (
                         id, change_id, number, title, body, files_json, consumes, produces,
                         verify_command, expected_output, covers_json, status,
                         created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', ?12, ?12)",
                    rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        &change_id,
                        &task.number,
                        &task.title,
                        task.body.as_deref(),
                        serde_json::to_string(&task.files)?,
                        task.consumes.as_deref(),
                        task.produces.as_deref(),
                        &task.verify_command,
                        &task.expected_output,
                        serde_json::to_string(&task.covers)?,
                        &now,
                    ],
                )?;
            }

            tx.execute(
                "UPDATE sdd_changes SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![enum_to_sql(&ChangeStatus::Planned)?, &now, &change_id],
            )?;

            Ok(PlanReport {
                tasks: command.tasks.len(),
                status: ChangeStatus::Planned,
            })
        })
    }

    /// Folds a change's deltas into the live base and moves it to `archived`.
    ///
    /// This is the step the other three exist for. Until it runs, a change's
    /// deltas are a proposal and `capabilities`, `requirements` and
    /// `scenarios` still describe the product as it was; after it, they
    /// describe the product as it is, and the next change reads them as its
    /// starting point. Every delta lands in one transaction for that reason —
    /// a half-applied archive is a live base describing a product that never
    /// existed, and nothing downstream can tell which half landed.
    ///
    /// A `removed` delta retires its requirement rather than deleting it:
    /// `requirements.status` exists so that a withdrawn requirement stays
    /// legible as a decision that was taken.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Domain`] for an invalid command,
    /// [`StoreError::NoWorkspace`] when the context names no workspace,
    /// [`StoreError::ChangeNotFound`] or [`StoreError::ChangeClosed`] when the
    /// change cannot be archived at all, [`StoreError::ChangeNotExecuted`]
    /// when a task of it is still open or it was never planned,
    /// [`StoreError::NothingToArchive`] when it proposes no deltas and did not
    /// declare `skip_specs`, [`StoreError::CapabilityPurposeRequired`] when a
    /// delta creating a capability carries no purpose, or a database error.
    pub fn archive(
        &self,
        command: &ArchiveCommand,
        context: &FactContext,
    ) -> Result<ArchiveReport, StoreError> {
        command.validate()?;

        // Outside the closure, as in the three verbs above.
        let workspace_id = context.workspace_id.ok_or(StoreError::NoWorkspace)?;

        self.execute_operation("archive", command.operation_id, command, |tx| {
            let change_id = command.change.to_string();
            let workspace = workspace_id.to_string();
            let (namespace, skip_specs) =
                require_archivable_change(tx, command.change, &workspace)?;
            require_tasks_closed(tx, command.change, &change_id, skip_specs)?;

            let deltas = load_deltas(tx, &change_id)?;
            if deltas.is_empty() && !skip_specs {
                return Err(StoreError::NothingToArchive(command.change));
            }

            let now = Utc::now().to_rfc3339();
            let mut report = ArchiveReport {
                added: 0,
                modified: 0,
                removed: 0,
                renamed: 0,
                capabilities_created: Vec::new(),
                status: ChangeStatus::Archived,
            };

            for delta in &deltas {
                apply_delta(
                    tx,
                    &workspace,
                    namespace.as_deref(),
                    delta,
                    &now,
                    &mut report,
                )?;
            }

            tx.execute(
                "UPDATE sdd_changes SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![enum_to_sql(&ChangeStatus::Archived)?, &now, &change_id],
            )?;

            Ok(report)
        })
    }
}

/// The `sdd_changes` columns behind [`ChangeSummary`], plus its counts —
/// shared by [`Store::open_changes`] and [`Store::change_detail`] so the two
/// cannot drift into showing a different shape of the same row.
///
/// The counts are correlated subqueries rather than a join: a join fanning
/// out over a change's deltas and its tasks at once would multiply the two
/// counts together, so getting the right numbers back out would mean
/// `COUNT(DISTINCT ...)` on both — no cheaper to read than two subqueries,
/// and easier to get wrong.
const CHANGE_SUMMARY_COLUMNS: &str = "
    id, slug, title, classification, status, updated_at,
    (SELECT COUNT(*) FROM spec_deltas WHERE spec_deltas.change_id = sdd_changes.id),
    (SELECT COUNT(*) FROM tasks WHERE tasks.change_id = sdd_changes.id),
    (SELECT COUNT(*) FROM tasks
      WHERE tasks.change_id = sdd_changes.id AND tasks.status IN ('done', 'skipped'))";

impl Store {
    // --- reading -------------------------------------------------------

    /// Open changes of this workspace and namespace, most recently touched
    /// first.
    ///
    /// "Open" excludes `archived` and `abandoned`: those are done, and the
    /// question this answers — "where did I leave off" — is about work still
    /// in flight. Read-only and does not go through `execute_operation`:
    /// nothing here mutates, so there is nothing for an `operation_id` to
    /// make idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NoWorkspace`] when the context names no
    /// workspace, or a database error.
    pub fn open_changes(&self, context: &FactContext) -> Result<Vec<ChangeSummary>, StoreError> {
        let workspace_id = context.workspace_id.ok_or(StoreError::NoWorkspace)?;

        let connection = self.lock()?;
        let sql = format!(
            "SELECT {CHANGE_SUMMARY_COLUMNS} FROM sdd_changes
             WHERE workspace_id = ?1 AND namespace IS ?2
               AND status NOT IN ('archived', 'abandoned')
             ORDER BY updated_at DESC"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement
            .query_map(
                rusqlite::params![workspace_id.to_string(), context.namespace.as_deref()],
                row_to_change_summary_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(ChangeSummaryRow::into_summary)
            .collect()
    }

    /// The full content of one change, or `None` if this workspace has none
    /// by that id.
    ///
    /// `None` rather than an error: a caller that has lost every other detail
    /// of a change and is asking about the id it still has is asking a
    /// legitimate question, and answering it with a refusal would read as a
    /// fault in the store rather than as "not this one." An id from another
    /// workspace is answered the same way `require_open_change` answers it
    /// for the write side: it does not exist as far as this workspace is
    /// concerned.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NoWorkspace`] when the context names no
    /// workspace, or a database error.
    pub fn change_detail(
        &self,
        change: ChangeId,
        context: &FactContext,
    ) -> Result<Option<ChangeDetail>, StoreError> {
        let workspace_id = context.workspace_id.ok_or(StoreError::NoWorkspace)?;

        let mut connection = self.lock()?;
        let tx = connection.transaction()?;

        let sql = format!(
            "SELECT {CHANGE_SUMMARY_COLUMNS} FROM sdd_changes WHERE id = ?1 AND workspace_id = ?2"
        );
        let row = tx
            .query_row(
                &sql,
                rusqlite::params![change.to_string(), workspace_id.to_string()],
                row_to_change_summary_row,
            )
            .optional()?;

        let Some(row) = row else {
            return Ok(None);
        };
        let summary = row.into_summary()?;

        let change_id = change.to_string();
        let proposal = load_proposal_document(&tx, &change_id)?;
        let deltas = load_delta_summaries(&tx, &change_id)?;
        let tasks = load_task_summaries(&tx, &change_id)?;
        drop(tx);

        Ok(Some(ChangeDetail {
            id: summary.id,
            slug: summary.slug,
            title: summary.title,
            classification: summary.classification,
            status: summary.status,
            updated_at: summary.updated_at,
            delta_count: summary.delta_count,
            task_count: summary.task_count,
            tasks_closed: summary.tasks_closed,
            why: proposal.why,
            what_changes: proposal.what_changes,
            capabilities: proposal.capabilities,
            impact: proposal.impact,
            deltas,
            tasks,
        }))
    }
}

/// Rewrites the proposal of the change the slug already names, and hands back
/// its id so a rewrite is indistinguishable from the first call to the caller.
///
/// Only `drafting` and `specified` may be rewritten. Past that the proposal
/// has already been broken down into a plan, and moving the agreement out from
/// under work someone is doing is a substitution rather than a correction, so
/// the slug is reported as taken.
///
/// The status is deliberately left alone: a change that has written specs
/// stays `specified`, because correcting the document the specs were written
/// against does not un-write them. What moves it back is `Store::specify`,
/// which owns that decision for the deltas it writes.
fn rewrite_proposal(
    tx: &Transaction<'_>,
    change_id: &str,
    status: ChangeStatus,
    command: &ProposeCommand,
    now: &str,
) -> Result<ProposeReport, StoreError> {
    if !matches!(status, ChangeStatus::Drafting | ChangeStatus::Specified) {
        return Err(StoreError::SlugTaken(command.slug.clone()));
    }
    require_no_stranded_deltas(tx, change_id, command)?;

    let declared_capabilities_owned = declared_capabilities(tx, change_id)?;

    // A rewrite that moves the capability list moves the contract the specs
    // were written against, so the change goes back to `drafting` and has to
    // be specified again — the same reasoning that pulls a `planned` change
    // back to `specified` when its spec changes. Correcting a title or a
    // rationale invalidates nothing and leaves the status alone.
    // Compared as sets: a proposal that lists the same capabilities in another
    // order restates the contract rather than moving it, and sending the
    // change back to `drafting` for that would cost a re-specification nobody
    // asked for. `require_no_stranded_deltas` below compares the same way.
    let declared: HashSet<&str> = declared_capabilities_owned
        .iter()
        .map(String::as_str)
        .collect();
    let offered: HashSet<&str> = command.capabilities.iter().map(String::as_str).collect();
    let status = if declared == offered {
        status
    } else {
        ChangeStatus::Drafting
    };

    tx.execute(
        "UPDATE sdd_changes
         SET title = ?1, classification = ?2, skip_specs = ?3, status = ?4, updated_at = ?5
         WHERE id = ?6",
        rusqlite::params![
            &command.title,
            enum_to_sql(&command.classification)?,
            command.skip_specs,
            enum_to_sql(&status)?,
            now,
            change_id,
        ],
    )?;
    write_proposal(tx, change_id, command, now)?;

    Ok(ProposeReport {
        id: parse_id(change_id)?,
        slug: command.slug.clone(),
        status,
        rewritten: true,
    })
}

/// The capability paths the change's proposal currently names.
///
/// Empty when there is no proposal row, which `Store::propose` makes
/// unreachable for a change it wrote: the change and its proposal land in one
/// transaction.
fn declared_capabilities(tx: &Transaction<'_>, change_id: &str) -> Result<Vec<String>, StoreError> {
    let body_json: Option<String> = tx
        .query_row(
            "SELECT body_json FROM sdd_artifacts WHERE change_id = ?1 AND kind = 'proposal'",
            [change_id],
            |row| row.get(0),
        )
        .optional()?;

    Ok(match body_json {
        Some(body_json) => serde_json::from_str::<DeclaredCapabilities>(&body_json)?.capabilities,
        None => Vec::new(),
    })
}

/// Refuses a rewrite that would leave deltas filed under a capability the
/// proposal no longer declares.
///
/// The other answer — accept it and let the deltas sit there orphaned — loses
/// text somebody wrote and reports success, and this store treats a silent
/// loss as the worse outcome every time it has to choose (see
/// `CapabilityPurposeRedundant`, which refuses for the same reason). Deltas
/// are compared directly rather than the two capability lists, because what
/// matters is not which declaration disappeared but whether anything was
/// written against it: dropping a capability nobody specified yet is an
/// ordinary correction and passes.
fn require_no_stranded_deltas(
    tx: &Transaction<'_>,
    change_id: &str,
    command: &ProposeCommand,
) -> Result<(), StoreError> {
    let declared: HashSet<&str> = command.capabilities.iter().map(String::as_str).collect();

    // Ordered so that a caller re-reading the refusal after a partial fix sees
    // the same list in the same order, as `require_full_coverage` does.
    let mut statement = tx.prepare(
        "SELECT DISTINCT capability_path FROM spec_deltas
         WHERE change_id = ?1 ORDER BY capability_path",
    )?;
    let stranded = statement
        .query_map([change_id], |row| row.get::<_, String>(0))?
        .filter(|path| match path {
            Ok(path) => !declared.contains(path.as_str()),
            Err(_) => true,
        })
        .collect::<Result<Vec<_>, _>>()?;

    if !stranded.is_empty() {
        return Err(StoreError::CapabilityDeltasStranded(stranded));
    }

    Ok(())
}

/// Writes the change's proposal document, replacing one already there.
///
/// An upsert rather than an insert and an update side by side: the two callers
/// differ in whether a row exists, and `sdd_artifacts_kind` already states
/// that at most one can. The row keeps its id and its `created_at` through a
/// rewrite — it is the same proposal, corrected.
fn write_proposal(
    tx: &Transaction<'_>,
    change_id: &str,
    command: &ProposeCommand,
    now: &str,
) -> Result<(), StoreError> {
    let body = ProposalBody {
        why: &command.why,
        what_changes: &command.what_changes,
        capabilities: &command.capabilities,
        impact: command.impact.as_deref(),
    };
    let body_json = serde_json::to_string(&body)?;

    tx.execute(
        "INSERT INTO sdd_artifacts (id, change_id, kind, body_json, created_at, updated_at)
         VALUES (?1, ?2, 'proposal', ?3, ?4, ?4)
         ON CONFLICT (change_id, kind)
         DO UPDATE SET body_json = excluded.body_json, updated_at = excluded.updated_at",
        rusqlite::params![uuid::Uuid::new_v4().to_string(), change_id, &body_json, now],
    )?;

    Ok(())
}

/// One row of `spec_deltas`, as archiving needs to read it.
///
/// `created_at` and the counts the report keeps are not here: this is the
/// instruction, not the record of carrying it out.
struct DeltaRow {
    id: String,
    capability_path: String,
    capability_id: Option<String>,
    purpose: Option<String>,
    op: DeltaOp,
    requirement_id: Option<String>,
    name: String,
    text: Option<String>,
    rename_to: Option<String>,
}

/// Reads the change being archived and refuses one that is already closed.
///
/// Hands back the namespace, which a delta creating a capability needs, and
/// `skip_specs`, which decides whether an empty change is legitimate. Scoped
/// by workspace like its two siblings: an id from another workspace does not
/// exist as far as this caller is concerned.
///
/// Unlike `require_plannable_change` there is no list of statuses that may
/// archive. Planning replaces rows that carry evidence of work, so it has to
/// care where the change stands; archiving only reads deltas nothing has
/// applied yet, and a change is either still open or it is not.
fn require_archivable_change(
    tx: &Transaction<'_>,
    change: ChangeId,
    workspace_id: &str,
) -> Result<(Option<String>, bool), StoreError> {
    let row: Option<(String, Option<String>, bool)> = tx
        .query_row(
            "SELECT status, namespace, skip_specs FROM sdd_changes
             WHERE id = ?1 AND workspace_id = ?2",
            rusqlite::params![change.to_string(), workspace_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    let (status, namespace, skip_specs) = row.ok_or(StoreError::ChangeNotFound(change))?;
    let status: ChangeStatus = enum_from_sql(&status)?;
    if matches!(status, ChangeStatus::Archived | ChangeStatus::Abandoned) {
        return Err(StoreError::ChangeClosed(change));
    }

    Ok((namespace, skip_specs))
}

/// Refuses to archive a change whose work is not finished.
///
/// `skipped` counts as closed: a task deliberately passed over is a decision
/// somebody took, unlike one still `pending` or `running`. A change with no
/// tasks at all is refused too, unless it was proposed with `skip_specs` —
/// planning may legitimately never have happened for one of those, whereas a
/// change that wrote specs and no plan has had nothing done to it.
fn require_tasks_closed(
    tx: &Transaction<'_>,
    change: ChangeId,
    change_id: &str,
    skip_specs: bool,
) -> Result<(), StoreError> {
    // Ordered so that a caller re-reading the refusal after finishing one task
    // sees the rest in the same order rather than a reshuffled list.
    let mut statement = tx.prepare(
        "SELECT number FROM tasks
         WHERE change_id = ?1 AND status NOT IN ('done', 'skipped')
         ORDER BY number",
    )?;
    let open = statement
        .query_map([change_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    if !open.is_empty() {
        return Err(StoreError::ChangeNotExecuted {
            change,
            tasks: open,
        });
    }

    if !skip_specs {
        let planned: i64 = tx.query_row(
            "SELECT COUNT(*) FROM tasks WHERE change_id = ?1",
            [change_id],
            |row| row.get(0),
        )?;
        if planned == 0 {
            return Err(StoreError::ChangeNotExecuted {
                change,
                tasks: Vec::new(),
            });
        }
    }

    Ok(())
}

/// The change's deltas, in the order they will be applied.
///
/// `created_at` then `name`, the ordering `require_full_coverage` already
/// uses: every delta of one `specify` call shares a timestamp, so the name is
/// what makes the order of two of them the same on every run.
fn load_deltas(tx: &Transaction<'_>, change_id: &str) -> Result<Vec<DeltaRow>, StoreError> {
    let mut statement = tx.prepare(
        "SELECT id, capability_path, capability_id, purpose, op, requirement_id,
                name, text, rename_to
         FROM spec_deltas WHERE change_id = ?1
         ORDER BY created_at, name",
    )?;

    let rows = statement
        .query_map([change_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    rows.into_iter()
        .map(|row| {
            Ok(DeltaRow {
                id: row.0,
                capability_path: row.1,
                capability_id: row.2,
                purpose: row.3,
                op: enum_from_sql(&row.4)?,
                requirement_id: row.5,
                name: row.6,
                text: row.7,
                rename_to: row.8,
            })
        })
        .collect()
}

/// Folds one delta into the live base and counts it in the report.
fn apply_delta(
    tx: &Transaction<'_>,
    workspace_id: &str,
    namespace: Option<&str>,
    delta: &DeltaRow,
    now: &str,
    report: &mut ArchiveReport,
) -> Result<(), StoreError> {
    match delta.op {
        DeltaOp::Added => {
            let capability_id = ensure_capability(tx, workspace_id, namespace, delta, now, report)?;
            let requirement_id = uuid::Uuid::new_v4().to_string();

            // `text` is passed as it stands rather than defaulted: the column
            // is NOT NULL and `magent-core` refuses an addition without text,
            // so a NULL here is a row nothing in this crate can write, and a
            // constraint failure says so far more usefully than a requirement
            // silently filed with no text at all.
            tx.execute(
                "INSERT INTO requirements
                     (id, capability_id, name, text, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'live', ?5, ?5)",
                rusqlite::params![
                    &requirement_id,
                    &capability_id,
                    &delta.name,
                    delta.text.as_deref(),
                    now,
                ],
            )?;
            copy_scenarios(tx, &delta.id, &requirement_id)?;
            report.added += 1;
        }
        DeltaOp::Modified => {
            let requirement_id = patched_requirement(delta)?;
            // `specify` checked this requirement was live, but that was then.
            // Archiving is the only place the live base moves, and the window
            // between the two calls is wide enough for another change to have
            // been proposed, specified and archived — retiring this very
            // requirement. Without the guard the update lands on the retired
            // row and leaves it carrying fresh text nobody agreed to ship.
            let patched = tx.execute(
                "UPDATE requirements SET text = ?1, updated_at = ?2
                 WHERE id = ?3 AND status = 'live'",
                rusqlite::params![delta.text.as_deref(), now, requirement_id],
            )?;
            if patched == 0 {
                return Err(StoreError::RequirementNotFound {
                    requirement_id: requirement_id.to_owned(),
                    capability_path: delta.capability_path.clone(),
                });
            }

            // Replaced rather than merged, which is what "a delta carries the
            // whole requirement, not a diff" means once it reaches the live
            // base: a scenario the author dropped when rewriting the
            // requirement has to disappear, and one kept is written again.
            tx.execute(
                "DELETE FROM scenarios WHERE requirement_id = ?1",
                [requirement_id],
            )?;
            copy_scenarios(tx, &delta.id, requirement_id)?;
            report.modified += 1;
        }
        DeltaOp::Removed => {
            tx.execute(
                "UPDATE requirements SET status = 'removed', updated_at = ?1 WHERE id = ?2",
                rusqlite::params![now, patched_requirement(delta)?],
            )?;
            report.removed += 1;
        }
        DeltaOp::Renamed => {
            let requirement_id = patched_requirement(delta)?;
            // Guarded for the same reason as `Modified`. `Removed` is not:
            // retiring a requirement that another change already retired
            // changes nothing and needs no argument.
            let renamed = tx.execute(
                "UPDATE requirements SET name = ?1, updated_at = ?2
                 WHERE id = ?3 AND status = 'live'",
                rusqlite::params![delta.rename_to.as_deref(), now, requirement_id],
            )?;
            if renamed == 0 {
                return Err(StoreError::RequirementNotFound {
                    requirement_id: requirement_id.to_owned(),
                    capability_path: delta.capability_path.clone(),
                });
            }
            report.renamed += 1;
        }
    }

    Ok(())
}

/// The requirement a `modified`, `removed` or `renamed` delta patches.
///
/// `magent-core` refuses all three without an id and `specify` checked that id
/// against the capability's live requirements, so this cannot be `None` for a
/// row this crate wrote. Named as an error rather than defaulted anyway,
/// because the alternative is an `UPDATE ... WHERE id IS NULL`, which matches
/// nothing, changes nothing and would still be counted in the report as
/// applied.
fn patched_requirement(delta: &DeltaRow) -> Result<&str, StoreError> {
    delta
        .requirement_id
        .as_deref()
        .ok_or_else(|| StoreError::RequirementNotFound {
            requirement_id: format!("(none, on delta {:?})", delta.name),
            capability_path: delta.capability_path.clone(),
        })
}

/// The capability an `added` delta files its requirement under, created if it
/// is not there yet.
///
/// The delta's own `capability_id` is trusted when set, and the table is
/// consulted again when it is not: `specify` left it NULL because the
/// capability did not exist *then*, and by now an earlier delta of this same
/// change — or another change archived in between — may have created it.
fn ensure_capability(
    tx: &Transaction<'_>,
    workspace_id: &str,
    namespace: Option<&str>,
    delta: &DeltaRow,
    now: &str,
    report: &mut ArchiveReport,
) -> Result<String, StoreError> {
    if let Some(capability_id) = &delta.capability_id {
        return Ok(capability_id.clone());
    }

    // Mirrors capabilities_path (0007_sdd.sql), NULL namespace folded the
    // same way `resolve_capability` folds it.
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT id, purpose FROM capabilities
             WHERE workspace_id = ?1 AND namespace IS ?2 AND path = ?3",
            rusqlite::params![workspace_id, namespace, &delta.capability_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((capability_id, recorded)) = existing {
        // Two deltas may name one capability that did not exist when either
        // was written — `specify` cannot refuse a redundant purpose for a row
        // that is not there yet, so both carry one and the second would lose
        // its own the moment the first created the row. Refusing here is the
        // rule `CapabilityPurposeRedundant` already states, arriving as late
        // as it can: silently keeping whichever delta sorted first would
        // discard text somebody wrote and report success.
        if delta
            .purpose
            .as_deref()
            .is_some_and(|offered| offered != recorded)
        {
            return Err(StoreError::CapabilityPurposeRedundant(
                delta.capability_path.clone(),
            ));
        }
        return Ok(capability_id);
    }

    // `specify` demands a purpose for a capability it cannot find, so this is
    // the same refusal arriving late rather than a new rule. Reached only if
    // the capability was deleted between the two calls — and filling the
    // NOT NULL column with a placeholder instead is `OpenSpec`'s "TBD", which
    // nobody ever circles back to.
    let purpose = delta
        .purpose
        .as_deref()
        .ok_or_else(|| StoreError::CapabilityPurposeRequired(delta.capability_path.clone()))?;

    let capability_id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO capabilities
             (id, workspace_id, namespace, path, purpose, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        rusqlite::params![
            &capability_id,
            workspace_id,
            namespace,
            &delta.capability_path,
            purpose,
            now,
        ],
    )?;
    report
        .capabilities_created
        .push(delta.capability_path.clone());

    Ok(capability_id)
}

/// Writes a delta's scenarios against a live requirement, keeping `seq`.
///
/// The rows in `delta_scenarios` stay where they are: they are what the change
/// proposed, and an archived change whose scenarios had been moved out from
/// under it would be a record of a decision with the decision removed.
fn copy_scenarios(
    tx: &Transaction<'_>,
    delta_id: &str,
    requirement_id: &str,
) -> Result<(), StoreError> {
    let mut statement = tx.prepare(
        "SELECT seq, name, given_text, when_text, then_text FROM delta_scenarios
         WHERE delta_id = ?1 ORDER BY seq",
    )?;
    let scenarios = statement
        .query_map([delta_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    for (seq, name, given_text, when_text, then_text) in scenarios {
        tx.execute(
            "INSERT INTO scenarios
                 (id, requirement_id, seq, name, given_text, when_text, then_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                requirement_id,
                seq,
                &name,
                given_text.as_deref(),
                &when_text,
                &then_text,
            ],
        )?;
    }

    Ok(())
}

/// Reads the change a plan is for and refuses one that cannot take a plan.
///
/// Scoped by workspace for the same reason `require_open_change` is: an id from
/// another workspace does not exist as far as this caller is concerned. It does
/// not reuse that helper because what it needs from the row is different — the
/// status and `skip_specs`, not the namespace — and reading the row twice to
/// share a function would cost more than it saved.
///
/// `specified` is the ordinary starting point and `planned` is a re-plan.
/// `drafting` is refused *unless* the change was proposed with `skip_specs`: a
/// change that deliberately writes no specs never reaches `specified`, so
/// insisting on that status would leave it with no way to be planned at all.
fn require_plannable_change(
    tx: &Transaction<'_>,
    change: ChangeId,
    workspace_id: &str,
) -> Result<(), StoreError> {
    let row: Option<(String, bool)> = tx
        .query_row(
            "SELECT status, skip_specs FROM sdd_changes
             WHERE id = ?1 AND workspace_id = ?2",
            rusqlite::params![change.to_string(), workspace_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (raw_status, skip_specs) = row.ok_or(StoreError::ChangeNotFound(change))?;
    let status: ChangeStatus = enum_from_sql(&raw_status)?;
    if matches!(status, ChangeStatus::Archived | ChangeStatus::Abandoned) {
        return Err(StoreError::ChangeClosed(change));
    }

    let plannable = match status {
        ChangeStatus::Specified | ChangeStatus::Planned => true,
        ChangeStatus::Drafting => skip_specs,
        // `executing` and `ready` are past planning: the tasks are being
        // worked, and replacing them would delete rows carrying the evidence
        // of work already verified. `specify` pulls such a change back to
        // `specified` first, which is the path that keeps that evidence's
        // disappearance a decision rather than a side effect.
        _ => false,
    };
    if !plannable {
        return Err(StoreError::ChangeNotSpecified {
            change,
            status: raw_status,
        });
    }

    Ok(())
}

/// Refuses a plan that leaves a requirement of this change to nobody.
///
/// Compared against `spec_deltas.name` rather than `requirements.name`: a
/// task's `covers` holds the name the requirement was proposed under in *this*
/// change, fixed at the moment of planning (`0009_tasks.sql`). A `renamed`
/// delta moves the live name in `requirements` without touching the plan, so
/// matching there would report a perfectly covered requirement as uncovered
/// the first time one is renamed.
///
/// A change proposed with `skip_specs` has no deltas, so the query returns
/// nothing and the check passes without needing a branch of its own.
fn require_full_coverage(
    tx: &Transaction<'_>,
    change_id: &str,
    command: &PlanCommand,
) -> Result<(), StoreError> {
    let covered: HashSet<&str> = command
        .tasks
        .iter()
        .flat_map(|task| task.covers.iter().map(String::as_str))
        .collect();

    // Ordered so that a caller re-reading the refusal after a failed fix sees
    // the same list in the same order rather than a reshuffled one.
    let mut statement = tx.prepare(
        "SELECT name FROM spec_deltas WHERE change_id = ?1
         ORDER BY created_at, name",
    )?;
    let uncovered = statement
        .query_map([change_id], |row| row.get::<_, String>(0))?
        .filter(|name| match name {
            Ok(name) => !covered.contains(name.as_str()),
            Err(_) => true,
        })
        .collect::<Result<Vec<_>, _>>()?;

    if !uncovered.is_empty() {
        return Err(StoreError::RequirementsUncovered(uncovered));
    }

    Ok(())
}

/// Reads the change these deltas are for, refusing one that cannot take them,
/// and hands back its namespace.
///
/// Scoped by workspace: a change id from another workspace is not "someone
/// else's change" to this caller, it is one that does not exist. The namespace
/// comes back because the capability below belongs to the change's repository,
/// not to whichever directory the caller happens to be sitting in.
fn require_open_change(
    tx: &Transaction<'_>,
    change: ChangeId,
    workspace_id: &str,
) -> Result<Option<String>, StoreError> {
    let row: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT status, namespace FROM sdd_changes
             WHERE id = ?1 AND workspace_id = ?2",
            rusqlite::params![change.to_string(), workspace_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (status, namespace) = row.ok_or(StoreError::ChangeNotFound(change))?;
    let status: ChangeStatus = enum_from_sql(&status)?;
    if matches!(status, ChangeStatus::Archived | ChangeStatus::Abandoned) {
        return Err(StoreError::ChangeClosed(change));
    }

    Ok(namespace)
}

/// Refuses a spec filed against a capability the change's proposal never
/// declared.
///
/// The proposal's `capabilities` list is the contract between the two phases,
/// and nothing in the schema relates it to `spec_deltas.capability_path` — so
/// without this check a change can grow a spec for a capability nobody agreed
/// to touch and no later step notices. The declared paths travel with the
/// refusal because the caller cannot see them: they are inside `body_json`,
/// and a refusal that named only the rejected path would send it to read the
/// database to find out what it may write instead.
fn require_proposed_capability(
    tx: &Transaction<'_>,
    change_id: &str,
    command: &SpecifyCommand,
) -> Result<(), StoreError> {
    let body_json: Option<String> = tx
        .query_row(
            "SELECT body_json FROM sdd_artifacts WHERE change_id = ?1 AND kind = 'proposal'",
            [change_id],
            |row| row.get(0),
        )
        .optional()?;

    // No proposal row means no capability was declared, and the refusal says
    // so. `Store::propose` writes the change and its proposal in one
    // transaction, so this is only reachable for a change written around it —
    // and inventing a permissive default for that case would make the one
    // change nobody can account for the one nothing is checked against.
    let declared = match body_json {
        Some(body_json) => serde_json::from_str::<DeclaredCapabilities>(&body_json)?.capabilities,
        None => Vec::new(),
    };

    if !declared.iter().any(|path| path == &command.capability_path) {
        return Err(StoreError::CapabilityNotProposed {
            capability_path: command.capability_path.clone(),
            declared,
        });
    }

    Ok(())
}

/// Finds the capability the deltas are filed under, and settles whether the
/// command owes a purpose.
///
/// A missing row is not an error: naming a capability that does not exist yet
/// is how a change proposes one, which is why the delta's `capability_id`
/// stays NULL until the change is archived. What the row decides is the
/// purpose — required when there is nothing on record, refused when there
/// already is, so that neither is silently dropped.
fn resolve_capability(
    tx: &Transaction<'_>,
    workspace_id: &str,
    namespace: Option<&str>,
    command: &SpecifyCommand,
) -> Result<Option<String>, StoreError> {
    // Mirrors capabilities_path (0007_sdd.sql), NULL namespace folded the
    // same way.
    let capability_id: Option<String> = tx
        .query_row(
            "SELECT id FROM capabilities
             WHERE workspace_id = ?1 AND namespace IS ?2 AND path = ?3",
            rusqlite::params![workspace_id, namespace, &command.capability_path],
            |row| row.get(0),
        )
        .optional()?;

    match (capability_id.is_some(), command.purpose.is_some()) {
        (false, false) => Err(StoreError::CapabilityPurposeRequired(
            command.capability_path.clone(),
        )),
        (true, true) => Err(StoreError::CapabilityPurposeRedundant(
            command.capability_path.clone(),
        )),
        _ => Ok(capability_id),
    }
}

/// The requirement a delta patches, checked to be a live one of this
/// capability.
///
/// `Added` names none: there is nothing yet to point at, so an id supplied
/// alongside it is not written. For the other three `magent-core` has already
/// insisted an id is present, and what is left is whether it is *this*
/// capability's — a foreign key would happily accept another's — and whether
/// it is still live. A retired requirement is kept rather than deleted, and
/// none of the three ops means anything against one: modifying, removing or
/// renaming something already withdrawn changes nothing that ships.
fn resolve_requirement<'a>(
    tx: &Transaction<'_>,
    capability_id: Option<&str>,
    requirement: &'a RequirementDraft,
    command: &SpecifyCommand,
) -> Result<Option<&'a str>, StoreError> {
    let requirement_id = match requirement.op {
        // Nothing to resolve, and nothing dropped on the floor either:
        // `RequirementDraft::validate` refuses an addition that carries an id
        // at all, so by the time it reaches here there is none to lose.
        DeltaOp::Added => return Ok(None),
        DeltaOp::Modified | DeltaOp::Removed | DeltaOp::Renamed => {
            requirement.requirement_id.as_deref()
        }
    };

    if let Some(id) = requirement_id {
        // `capability_id IS NULL` matches nothing, since the column is NOT
        // NULL — so patching a requirement of a capability that does not exist
        // yet is refused here too.
        let belongs: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM requirements
                 WHERE id = ?1 AND capability_id IS ?2 AND status = 'live'",
                rusqlite::params![id, capability_id],
                |row| row.get(0),
            )
            .optional()?;
        if belongs.is_none() {
            return Err(StoreError::RequirementNotFound {
                requirement_id: id.to_owned(),
                capability_path: command.capability_path.clone(),
            });
        }
    }

    Ok(requirement_id)
}

/// Refuses a requirement name this change has already proposed for this
/// capability.
///
/// Calling `specify` again *adds* to a change's deltas; it does not replace
/// them, which is what makes a repeated name a collision rather than an
/// overwrite. A model refining a spec runs into this on the ordinary path, so
/// what it gets back has to be its own words rather than the name of the
/// `spec_deltas_identity` index. Duplicates *within* one command are already
/// `magent-core`'s to catch; this is the second call.
fn require_unproposed(
    tx: &Transaction<'_>,
    change_id: &str,
    command: &SpecifyCommand,
    requirement: &RequirementDraft,
) -> Result<(), StoreError> {
    let taken: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM spec_deltas
             WHERE change_id = ?1 AND capability_path = ?2 AND name = ?3",
            rusqlite::params![change_id, &command.capability_path, &requirement.name],
            |row| row.get(0),
        )
        .optional()?;

    if taken.is_some() {
        return Err(StoreError::DeltaAlreadyProposed {
            requirement_name: requirement.name.clone(),
            capability_path: command.capability_path.clone(),
        });
    }

    Ok(())
}

/// Writes one delta and the scenarios that belong to it.
fn write_delta(
    tx: &Transaction<'_>,
    change_id: &str,
    capability_id: Option<&str>,
    command: &SpecifyCommand,
    requirement: &RequirementDraft,
    requirement_id: Option<&str>,
    now: &str,
) -> Result<(), StoreError> {
    let delta_id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO spec_deltas (
             id, change_id, capability_path, capability_id, purpose, op,
             requirement_id, name, text, rename_to, reason, migration, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            &delta_id,
            change_id,
            &command.capability_path,
            capability_id,
            command.purpose.as_deref(),
            enum_to_sql(&requirement.op)?,
            requirement_id,
            &requirement.name,
            requirement.text.as_deref(),
            requirement.rename_to.as_deref(),
            requirement.reason.as_deref(),
            requirement.migration.as_deref(),
            now,
        ],
    )?;

    // The sequence is the order the caller wrote them in, made explicit: rows
    // come back in whatever order the query asks for, and a scenario list read
    // back shuffled is a different spec.
    for (seq, scenario) in (0i64..).zip(&requirement.scenarios) {
        tx.execute(
            "INSERT INTO delta_scenarios (
                 id, delta_id, seq, name, given_text, when_text, then_text
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                &delta_id,
                seq,
                &scenario.name,
                scenario.given.as_deref(),
                &scenario.when,
                &scenario.then,
            ],
        )?;
    }

    Ok(())
}

// --- reading -----------------------------------------------------------

/// One row of [`CHANGE_SUMMARY_COLUMNS`], read with named fields the way
/// `facts.rs`'s `row_to_parts` reads a fact: `classification` and `status`
/// are adjacent columns of the same type, and `task_count` and
/// `tasks_closed` are adjacent columns of another — exactly the kind of pair
/// a reordered `SELECT` could swap silently if this were a positional tuple.
struct ChangeSummaryRow {
    id: String,
    slug: String,
    title: String,
    classification: String,
    status: String,
    updated_at: String,
    delta_count: i64,
    task_count: i64,
    tasks_closed: i64,
}

impl ChangeSummaryRow {
    fn into_summary(self) -> Result<ChangeSummary, StoreError> {
        Ok(ChangeSummary {
            id: parse_id(&self.id)?,
            slug: self.slug,
            title: self.title,
            classification: enum_from_sql(&self.classification)?,
            status: enum_from_sql(&self.status)?,
            updated_at: parse_timestamp(&self.updated_at)?,
            delta_count: count_to_usize(self.delta_count),
            task_count: count_to_usize(self.task_count),
            tasks_closed: count_to_usize(self.tasks_closed),
        })
    }
}

fn row_to_change_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeSummaryRow> {
    Ok(ChangeSummaryRow {
        id: row.get(0)?,
        slug: row.get(1)?,
        title: row.get(2)?,
        classification: row.get(3)?,
        status: row.get(4)?,
        updated_at: row.get(5)?,
        delta_count: row.get(6)?,
        task_count: row.get(7)?,
        tasks_closed: row.get(8)?,
    })
}

/// `COUNT(*)` never returns a negative number, so the only way this loses
/// information is on a database holding more rows than fit in a `usize` —
/// not a change this store will ever see.
fn count_to_usize(count: i64) -> usize {
    usize::try_from(count).unwrap_or(0)
}

/// What `sdd_artifacts.body_json` holds for a `kind = 'proposal'` row, read
/// back rather than written — the owned counterpart to [`ProposalBody`],
/// which borrows and so cannot outlive the query that built it.
#[derive(Deserialize)]
struct ProposalDocument {
    why: String,
    #[serde(default)]
    what_changes: Vec<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    impact: Option<String>,
}

/// The change's proposal document, parsed.
///
/// Unlike [`declared_capabilities`], which treats a missing row as
/// legitimate for a rewrite in progress, this is only ever called after
/// [`Store::change_detail`] has already confirmed the change exists — and
/// `Store::propose` writes a change and its proposal in one transaction, so a
/// change with no proposal row is not reachable here.
fn load_proposal_document(
    tx: &Transaction<'_>,
    change_id: &str,
) -> Result<ProposalDocument, StoreError> {
    let body_json: String = tx.query_row(
        "SELECT body_json FROM sdd_artifacts WHERE change_id = ?1 AND kind = 'proposal'",
        [change_id],
        |row| row.get(0),
    )?;

    Ok(serde_json::from_str(&body_json)?)
}

/// One row of `spec_deltas`, as [`Store::change_detail`] shows it.
struct DeltaSummaryRow {
    op: String,
    name: String,
    capability_path: String,
}

/// The change's deltas, oldest first — the same ordering [`load_deltas`]
/// uses for archiving, so a caller reading a change mid-flight sees them in
/// the order they will eventually be applied.
fn load_delta_summaries(
    tx: &Transaction<'_>,
    change_id: &str,
) -> Result<Vec<DeltaSummary>, StoreError> {
    let mut statement = tx.prepare(
        "SELECT op, name, capability_path FROM spec_deltas
         WHERE change_id = ?1 ORDER BY created_at, name",
    )?;
    let rows = statement
        .query_map([change_id], |row| {
            Ok(DeltaSummaryRow {
                op: row.get(0)?,
                name: row.get(1)?,
                capability_path: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    rows.into_iter()
        .map(|row| {
            Ok(DeltaSummary {
                op: enum_from_sql(&row.op)?,
                name: row.name,
                capability_path: row.capability_path,
            })
        })
        .collect()
}

/// One row of `tasks`, as [`Store::change_detail`] shows it.
struct TaskSummaryRow {
    number: String,
    title: String,
    status: String,
    verify_command: String,
}

/// The change's tasks, in task-number order — the same ordering
/// `require_tasks_closed` reads them in.
fn load_task_summaries(
    tx: &Transaction<'_>,
    change_id: &str,
) -> Result<Vec<TaskSummary>, StoreError> {
    let mut statement = tx.prepare(
        "SELECT number, title, status, verify_command FROM tasks
         WHERE change_id = ?1 ORDER BY number",
    )?;
    let rows = statement
        .query_map([change_id], |row| {
            Ok(TaskSummaryRow {
                number: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                verify_command: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|row| TaskSummary {
            number: row.number,
            title: row.title,
            status: row.status,
            verify_command: row.verify_command,
        })
        .collect())
}
