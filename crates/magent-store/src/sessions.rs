//! Binding a harness session to a Magent run.
//!
//! Hooks only know the harness's own session id. Everything they record has to
//! be attributed through that id, so this module owns the mapping between
//! `sessions.external_session_hint` and a run.

use std::path::Path;

use chrono::Utc;
use magent_core::{
    CheckpointOrigin, FileLedgerEntry, HarnessKind, RunId, RunSnapshot, SessionId, StartRunCommand,
    StartRunResult, WorkflowStage, WorkspaceId,
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

    /// Opens a run, or joins the one already in flight for this workspace.
    ///
    /// The hook layer opens a run on the first prompt, so by the time the model
    /// calls `magent_start` there is usually one already. Creating a second
    /// would split one task across two runs: the model would checkpoint into
    /// one while the hooks recorded edits and the restoration packet into the
    /// other, and neither would be complete.
    ///
    /// An explicit `resume_run_id` always wins, since that names a specific run.
    ///
    /// # Errors
    /// Fails on a database error, or if the named run cannot be resumed.
    pub fn adopt_or_start_run(
        &self,
        command: &StartRunCommand,
        harness: HarnessKind,
    ) -> Result<StartRunResult, StoreError> {
        if command.resume_run_id.is_some() {
            return self.start_run(command, harness);
        }

        let Some(root) = command.workspace_roots.first() else {
            return self.start_run(command, harness);
        };

        let in_flight = self.latest_open_run_for_path(root)?;
        let Some(existing) = in_flight else {
            return self.start_run(command, harness);
        };

        let result = self.start_run(
            &StartRunCommand {
                resume_run_id: Some(existing.run_id),
                ..command.clone()
            },
            harness,
        )?;

        // The hook titles a run from the first raw prompt, which is the symptom
        // rather than the task. When the model then states what it is doing,
        // that is the better name and it replaces the derived one.
        //
        // Only here: an explicit resume_run_id means joining a run that is
        // already named, and renaming it from a passing description would be
        // rewriting someone else's work.
        let refined = command.task.trim();
        if !refined.is_empty() && refined != result.task {
            let connection = self.lock()?;
            connection.execute(
                "UPDATE runs SET task = ?1, updated_at = ?2 WHERE id = ?3",
                (
                    refined,
                    Utc::now().to_rfc3339(),
                    existing.run_id.to_string(),
                ),
            )?;

            return Ok(StartRunResult {
                task: refined.to_owned(),
                ..result
            });
        }

        Ok(result)
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

    /// The session most recently opened against `run_id` and not yet ended.
    ///
    /// A checkpoint belongs to a session, but the model has no way to learn its
    /// own session id: the server issued it. Rather than make it ask, the
    /// server answers that question itself.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn latest_open_session(&self, run_id: RunId) -> Result<Option<SessionId>, StoreError> {
        let connection = self.lock()?;
        let found = connection
            .query_row(
                "SELECT id FROM sessions
                 WHERE run_id = ?1 AND ended_at IS NULL
                 ORDER BY started_at DESC, rowid DESC
                 LIMIT 1",
                [run_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(found.and_then(|id| id.parse().ok()))
    }

    /// The run's current state plus its most recent checkpoint.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn snapshot(&self, run_id: RunId) -> Result<RunSnapshot, StoreError> {
        self.get_run(run_id)
    }

    /// Writes a checkpoint from observation, carrying forward prior reasoning.
    ///
    /// `PreCompact` runs between the decision to compact and the compaction, so
    /// it cannot wait for the distiller. It records what is provably true now.
    ///
    /// Crucially it inherits the reasoning from the run's previous checkpoint
    /// rather than replacing it with blanks. A decision does not become false
    /// because time passed, and this checkpoint is the one the restoration
    /// packet reads: dropping the reasoning here would mean the model's own
    /// account of the work vanished at exactly the moment it was needed.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn record_observed_checkpoint(
        &self,
        binding: SessionBinding,
        changed_files: Vec<String>,
        fallback_summary: &str,
    ) -> Result<(), StoreError> {
        use magent_core::{CheckpointCommand, OperationId};

        let run = self.get_run(binding.run_id)?;

        // `completed` is not a stage a checkpoint may claim; it is reached only
        // through magent_finish.
        let stage = if run.stage == WorkflowStage::Completed {
            WorkflowStage::Reviewing
        } else {
            run.stage
        };

        let previous = run.latest_checkpoint;
        let (
            completed_steps,
            next_steps,
            decisions,
            rejected,
            verification,
            risks,
            summary,
            origin,
        ) = previous.map_or_else(
            || {
                (
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    fallback_summary.to_owned(),
                    CheckpointOrigin::Deterministic,
                )
            },
            |checkpoint| {
                (
                    checkpoint.completed_steps,
                    checkpoint.next_steps,
                    checkpoint.decisions,
                    checkpoint.rejected,
                    checkpoint.verification,
                    checkpoint.risks,
                    checkpoint.handoff_summary,
                    // The reasoning is as good as the checkpoint it came
                    // from, so the label follows it.
                    checkpoint.origin,
                )
            },
        );

        self.save_checkpoint(&CheckpointCommand {
            operation_id: OperationId::new(),
            run_id: binding.run_id,
            session_id: binding.session_id,
            stage,
            origin,
            completed_steps,
            next_steps,
            decisions,
            rejected,
            changed_files,
            verification,
            risks,
            handoff_summary: summary,
            task_done: None,
            binding: None,
        })?;

        Ok(())
    }

    /// Takes the right to tell `session` a notice of `kind`, once.
    ///
    /// True means this caller may speak and nobody else will; false means the
    /// session has already been told. The insert succeeding *is* the claim —
    /// the uniqueness constraint decides it, not a preceding read — so two
    /// callers racing cannot both come away believing they may speak. That is
    /// why this is `claim_notice` rather than `should_notify`: it takes the
    /// right rather than asking whether the right is available, which is the
    /// difference between a race that cannot be lost and one that can.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn claim_notice(&self, session: &str, kind: &str) -> Result<bool, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let changed = tx.execute(
            "INSERT INTO session_notices (external_session_hint, kind, sent_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (external_session_hint, kind) DO NOTHING",
            (session, kind, Utc::now().to_rfc3339()),
        )?;
        tx.commit()?;

        Ok(changed == 1)
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
