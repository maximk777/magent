//! The spec-driven process, as rows.
//!
//! `magent-core` knows the shape a proposal must take but nothing about what
//! is already in the database, so the checks that depend on existing rows —
//! is this slug already live — belong here rather than there.

use chrono::Utc;
use magent_core::{ChangeId, ChangeStatus, ProposeCommand, Validate};
use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::{
    error::StoreError,
    facts::FactContext,
    store::{Store, enum_to_sql},
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
}
