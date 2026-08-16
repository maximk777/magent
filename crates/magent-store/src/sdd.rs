//! The spec-driven process, as rows.
//!
//! `magent-core` knows the shape a proposal must take but nothing about what
//! is already in the database, so the checks that depend on existing rows —
//! is this slug already live — belong here rather than there.

use chrono::Utc;
use magent_core::{
    ChangeId, ChangeStatus, DeltaOp, ProposeCommand, RequirementDraft, SpecifyCommand, Validate,
};
use rusqlite::{OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::{
    error::StoreError,
    facts::FactContext,
    store::{Store, enum_from_sql, enum_to_sql},
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

impl Store {
    /// Opens a change: one row in `sdd_changes` with status `drafting`, and
    /// one row in `sdd_artifacts` holding the proposal, written together.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Domain`] for an invalid command,
    /// [`StoreError::NoWorkspace`] when the context names no workspace to
    /// file the change under, [`StoreError::SlugTaken`] when the slug already
    /// names a change still in flight, or a database error.
    pub fn propose(
        &self,
        command: &ProposeCommand,
        context: &FactContext,
    ) -> Result<ChangeId, StoreError> {
        command.validate()?;

        // Resolved before the writer lock, for the same reason validation is:
        // the column is NOT NULL, so the database would refuse this anyway,
        // but it would refuse it by naming a constraint. What the caller has
        // to fix is the working directory, and only this layer knows that.
        let workspace_id = context.workspace_id.ok_or(StoreError::NoWorkspace)?;

        self.execute_operation("propose", command.operation_id, command, |tx| {
            // Mirrors sdd_changes_live_slug (0007_sdd.sql): a slug is taken
            // only by a change that has not yet been archived or abandoned.
            // Relying on the unique index itself to report this would surface
            // a raw "UNIQUE constraint failed" to the caller, which does not
            // say what to do about it.
            let taken: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM sdd_changes
                     WHERE workspace_id = ?1 AND namespace IS ?2 AND slug = ?3
                       AND status NOT IN ('archived', 'abandoned')",
                    rusqlite::params![
                        workspace_id.to_string(),
                        context.namespace.as_deref(),
                        &command.slug,
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Err(StoreError::SlugTaken(command.slug.clone()));
            }

            let now = Utc::now().to_rfc3339();
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

            let body = ProposalBody {
                why: &command.why,
                what_changes: &command.what_changes,
                capabilities: &command.capabilities,
                impact: command.impact.as_deref(),
            };
            let body_json = serde_json::to_string(&body)?;

            tx.execute(
                "INSERT INTO sdd_artifacts (id, change_id, kind, body_json, created_at, updated_at)
                 VALUES (?1, ?2, 'proposal', ?3, ?4, ?4)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    change_id.to_string(),
                    &body_json,
                    &now,
                ],
            )?;

            Ok(change_id)
        })
    }

    /// Attaches a capability's deltas to a change and moves it to
    /// `specified`.
    ///
    /// Every delta and every scenario lands in one transaction: a spec is one
    /// artifact, and half of it is not a smaller spec but an unreviewed one.
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
    /// change cannot take deltas, [`StoreError::CapabilityPurposeRequired`] or
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
