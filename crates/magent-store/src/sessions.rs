//! Binding a harness session to a Magent run.
//!
//! Hooks only know the harness's own session id. Everything they record has to
//! be attributed through that id, so this module owns the mapping between
//! `sessions.external_session_hint` and a run.

use std::path::Path;

use chrono::Utc;
use magent_core::{
    CheckpointOrigin, FileLedgerEntry, HarnessKind, RunId, RunSnapshot, SessionId, WorkflowStage,
    WorkspaceId,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};

use crate::{
    error::StoreError,
    git,
    store::{Store, enum_to_sql, parse_id, upsert_repository},
};

/// A harness session attached to a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionBinding {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    /// True when this binding had to open a new run rather than join one.
    pub opened_run: bool,
}

impl Store {
    /// The run this harness session is already attached to, if any.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn binding_for_external_session(
        &self,
        hint: &str,
    ) -> Result<Option<SessionBinding>, StoreError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT s.id, s.run_id, r.workspace_id
                 FROM sessions s JOIN runs r ON r.id = s.run_id
                 WHERE s.external_session_hint = ?1
                 ORDER BY s.started_at DESC LIMIT 1",
                [hint],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;

        row.map(|(session_id, run_id, workspace_id)| {
            Ok(SessionBinding {
                run_id: parse_id(&run_id)?,
                session_id: parse_id(&session_id)?,
                workspace_id: parse_id(&workspace_id)?,
                opened_run: false,
            })
        })
        .transpose()
    }

    /// Attaches this harness session to the workspace's most recent open run.
    ///
    /// Returns `None` when the workspace has no open run: announcing Magent in a
    /// session where nothing is in flight would spend context for no benefit.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn attach_to_open_run(
        &self,
        hint: &str,
        cwd: &Path,
        harness: HarnessKind,
    ) -> Result<Option<SessionBinding>, StoreError> {
        if let Some(existing) = self.binding_for_external_session(hint)? {
            return Ok(Some(existing));
        }

        // Probed before the transaction opens; see `Store::start_run`.
        let probe = git::discover(cwd);

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();

        let workspace_id = upsert_repository(&tx, &probe, &now)?.workspace_id;
        let Some(run_id) = latest_open_run(&tx, workspace_id)? else {
            drop(tx);
            return Ok(None);
        };

        let session_id = insert_session(&tx, run_id, harness, hint, &now)?;
        tx.commit()?;

        Ok(Some(SessionBinding {
            run_id,
            session_id,
            workspace_id,
            opened_run: false,
        }))
    }

    /// Attaches this harness session to a run, opening one titled `task` when
    /// the workspace has none in flight.
    ///
    /// This is what makes the hook layer trustworthy: a task is recorded because
    /// the user asked for work, not because the model chose to announce it.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn bind_session(
        &self,
        hint: &str,
        cwd: &Path,
        task: &str,
        harness: HarnessKind,
    ) -> Result<SessionBinding, StoreError> {
        if let Some(existing) = self.binding_for_external_session(hint)? {
            return Ok(existing);
        }

        let probe = git::discover(cwd);

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();

        let workspace_id = upsert_repository(&tx, &probe, &now)?.workspace_id;

        let (run_id, opened_run) = if let Some(run_id) = latest_open_run(&tx, workspace_id)? {
            (run_id, false)
        } else {
            let run_id = RunId::new();
            tx.execute(
                "INSERT INTO runs (id, workspace_id, task, status, stage, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?5)",
                (
                    run_id.to_string(),
                    workspace_id.to_string(),
                    truncate_task(task),
                    enum_to_sql(&WorkflowStage::Discovering)?,
                    &now,
                ),
            )?;
            (run_id, true)
        };

        let session_id = insert_session(&tx, run_id, harness, hint, &now)?;
        tx.commit()?;

        Ok(SessionBinding {
            run_id,
            session_id,
            workspace_id,
            opened_run,
        })
    }

    /// # Errors
    /// Fails on a database error.
    pub fn run_for_external_session(&self, hint: &str) -> Result<Option<RunId>, StoreError> {
        Ok(self
            .binding_for_external_session(hint)?
            .map(|binding| binding.run_id))
    }

    /// Records one observed mutation against whatever run this session belongs
    /// to. A session with no run silently records nothing.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn append_ledger_for_external_session(
        &self,
        hint: &str,
        entry: &FileLedgerEntry,
    ) -> Result<(), StoreError> {
        let Some(binding) = self.binding_for_external_session(hint)? else {
            return Ok(());
        };
        self.append_ledger(binding.run_id, binding.session_id, entry)
    }

    /// # Errors
    /// Fails on a database error.
    pub fn ledger_for_external_session(
        &self,
        hint: &str,
        limit: usize,
    ) -> Result<Vec<FileLedgerEntry>, StoreError> {
        match self.binding_for_external_session(hint)? {
            Some(binding) => self.ledger(binding.run_id, limit),
            None => Ok(Vec::new()),
        }
    }

    /// Ends the harness session. The run stays open: closing an editor is not
    /// finishing a task.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn close_external_session(&self, hint: &str) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE sessions SET ended_at = ?1
             WHERE external_session_hint = ?2 AND ended_at IS NULL",
            (Utc::now().to_rfc3339(), hint),
        )?;
        Ok(())
    }

    /// The most recently touched open run for whatever workspace `path`
    /// belongs to.
    ///
    /// Read-only, and registers the repository on first sight so that asking
    /// about an unknown directory answers "nothing in flight" rather than
    /// failing.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn latest_open_run_for_path(&self, path: &Path) -> Result<Option<RunSnapshot>, StoreError> {
        let resolved = self.resolve_workspace_for(path)?;

        // The guard is scoped so it is released before `get_run` runs. The
        // store's mutex is not reentrant, so holding it across a call to
        // another public method deadlocks — silently and totally, hanging the
        // MCP server or a hook rather than returning an error.
        let run_id = {
            let mut connection = self.lock()?;
            let tx = connection.transaction()?;
            let found = latest_open_run(&tx, resolved.workspace_id)?;
            drop(tx);
            found
        };

        run_id.map(|run_id| self.get_run(run_id)).transpose()
    }

    /// The run's current state plus its most recent checkpoint.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn snapshot(&self, run_id: RunId) -> Result<RunSnapshot, StoreError> {
        self.get_run(run_id)
    }

    /// Writes a checkpoint assembled from observation alone — no model call.
    ///
    /// `PreCompact` runs between the decision to compact and the compaction, so
    /// it cannot wait for the distiller. This records what is provably true now;
    /// the reasoning behind it is filled in later by an `enrich_checkpoint` job.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn record_observed_checkpoint(
        &self,
        binding: SessionBinding,
        changed_files: Vec<String>,
        verification: Vec<String>,
        summary: String,
    ) -> Result<(), StoreError> {
        use magent_core::{CheckpointCommand, OperationId};

        let stage = self.get_run(binding.run_id)?.stage;
        let stage = if stage == WorkflowStage::Completed {
            WorkflowStage::Reviewing
        } else {
            stage
        };

        self.save_checkpoint(&CheckpointCommand {
            operation_id: OperationId::new(),
            run_id: binding.run_id,
            session_id: binding.session_id,
            stage,
            origin: CheckpointOrigin::Deterministic,
            completed_steps: Vec::new(),
            next_steps: Vec::new(),
            decisions: Vec::new(),
            rejected: Vec::new(),
            changed_files,
            verification,
            risks: Vec::new(),
            handoff_summary: summary,
        })?;

        Ok(())
    }
}

/// The workspace's most recently touched open run.
fn latest_open_run(
    tx: &Transaction<'_>,
    workspace_id: WorkspaceId,
) -> Result<Option<RunId>, StoreError> {
    let row: Option<String> = tx
        .query_row(
            "SELECT id FROM runs WHERE workspace_id = ?1 AND status = 'open'
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
            [workspace_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;

    row.map(|id| parse_id(&id)).transpose()
}

fn insert_session(
    tx: &Transaction<'_>,
    run_id: RunId,
    harness: HarnessKind,
    hint: &str,
    now: &str,
) -> Result<SessionId, StoreError> {
    let session_id = SessionId::new();
    tx.execute(
        "INSERT INTO sessions (id, run_id, harness, external_session_hint, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            session_id.to_string(),
            run_id.to_string(),
            enum_to_sql(&harness)?,
            hint,
            now,
        ),
    )?;
    Ok(session_id)
}

/// A run title comes from a raw prompt, which can be a page long. The full text
/// stays in the transcript; the run only needs something recognisable.
fn truncate_task(task: &str) -> String {
    const LIMIT: usize = 160;

    let single_line = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }

    let truncated: String = single_line.chars().take(LIMIT).collect();
    format!("{}…", truncated.trim_end())
}
