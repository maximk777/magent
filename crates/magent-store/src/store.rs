use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use chrono::{DateTime, Utc};
use magent_core::{
    ChangeStatus, CheckpointCommand, CheckpointId, CheckpointOrigin, CheckpointResult,
    CheckpointSnapshot, FileLedgerEntry, FinishAction, FinishRunCommand, FinishRunResult, GitState,
    HarnessKind, OperationId, RepositoryId, RepositoryRole, RunId, RunSnapshot, RunStatus,
    SessionId, SpecBinding, StartRunCommand, StartRunResult, TaskClosed, TaskDone, Validate,
    WorkflowStage, WorkspaceId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    error::StoreError,
    git::{self, RepositoryProbe},
    migrations, sdd,
};

/// How long a writer waits for a competing writer before giving up.
///
/// With no daemon, several hook processes and the MCP server contend for the
/// same file. Five seconds is far beyond any legitimate write here, so hitting
/// it means something is wedged rather than merely busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// What a working directory resolved to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceResolution {
    pub workspace_id: WorkspaceId,
    pub repository_id: RepositoryId,
    /// See `repositories.identity_key` in the migration.
    pub identity_key: String,
    pub toplevel: PathBuf,
    pub origin_url: Option<String>,
    /// `None` when the path is not inside a git repository.
    pub git: Option<GitState>,
    /// How freely this repository may be touched.
    pub role: RepositoryRole,
}

/// Where a queued job currently stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobState {
    pub status: String,
    pub retry_remaining: i64,
    pub last_error: Option<String>,
}

/// A job handed to a worker, already leased.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub kind: String,
    pub job_key: String,
    pub payload_json: String,
    pub retry_remaining: i64,
}

/// The durable store. Owns one connection; concurrency across processes is
/// handled by `SQLite` in WAL mode rather than by a coordinating daemon.
pub struct Store {
    pub(crate) connection: Mutex<Connection>,
}

impl Store {
    /// Opens (creating if needed) the database at `path` and migrates it.
    ///
    /// # Errors
    ///
    /// Fails if the parent directory cannot be created, the file cannot be
    /// opened, or the schema is newer than this build understands.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Database(error.to_string()))?;
        }

        let mut connection = Connection::open(path)?;

        connection.busy_timeout(BUSY_TIMEOUT)?;
        ensure_wal(&connection)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;

        migrations::apply(&mut connection)?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::Database("store mutex was poisoned".into()))
    }

    // --- introspection -----------------------------------------------------

    /// # Errors
    /// Fails if the pragma cannot be read.
    pub fn journal_mode(&self) -> Result<String, StoreError> {
        let connection = self.lock()?;
        Ok(connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    /// # Errors
    /// Fails if the pragma cannot be read.
    pub fn foreign_keys_enabled(&self) -> Result<bool, StoreError> {
        let connection = self.lock()?;
        Ok(connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?)
    }

    /// How much background work is waiting, and how much gave up.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn job_counts(&self) -> Result<(usize, usize), StoreError> {
        let connection = self.lock()?;
        let count = |status: &str| -> Result<usize, StoreError> {
            let value: i64 = connection.query_row(
                "SELECT COUNT(*) FROM jobs WHERE status = ?1",
                [status],
                |row| row.get(0),
            )?;
            Ok(usize::try_from(value).unwrap_or(0))
        };

        Ok((count("pending")?, count("failed")?))
    }

    /// # Errors
    /// Fails if the migration table cannot be read.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        let connection = self.lock()?;
        migrations::read_version(&connection)
    }

    /// # Errors
    /// Fails on a database error.
    pub fn run_count(&self) -> Result<usize, StoreError> {
        self.scalar_count("SELECT COUNT(*) FROM runs", [])
    }

    /// # Errors
    /// Fails on a database error.
    pub fn repository_count(&self) -> Result<usize, StoreError> {
        self.scalar_count("SELECT COUNT(*) FROM repositories", [])
    }

    /// # Errors
    /// Fails on a database error.
    pub fn total_checkpoint_count(&self) -> Result<usize, StoreError> {
        self.scalar_count("SELECT COUNT(*) FROM checkpoints", [])
    }

    /// # Errors
    /// Fails on a database error.
    pub fn checkpoint_count(&self, run_id: RunId) -> Result<usize, StoreError> {
        self.scalar_count(
            "SELECT COUNT(*) FROM checkpoints WHERE run_id = ?1",
            [run_id.to_string()],
        )
    }

    fn scalar_count<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<usize, StoreError> {
        let connection = self.lock()?;
        let count: i64 = connection.query_row(sql, params, |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    // --- run lifecycle -----------------------------------------------------

    /// Opens a new run, or attaches a fresh session to an existing one.
    ///
    /// `harness` is supplied by the server, never by the client: a session must
    /// not be able to misreport which harness it is.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Domain`] for an invalid command,
    /// [`StoreError::RunNotFound`] or [`StoreError::RunClosed`] when resuming.
    pub fn start_run(
        &self,
        command: &StartRunCommand,
        harness: HarnessKind,
    ) -> Result<StartRunResult, StoreError> {
        command.validate()?;

        // Probed before the transaction opens. Each git subprocess costs tens
        // of milliseconds, and holding the single write lock for that long
        // would stall every concurrent hook.
        let probe = match command.resume_run_id {
            Some(_) => None,
            None => Some(git::discover(command.workspace_roots.first().ok_or(
                StoreError::Domain(magent_core::DomainError::MissingWorkspaceRoot),
            )?)),
        };

        self.execute_operation("start_run", command.operation_id, command, |tx| {
            let now = Utc::now().to_rfc3339();

            let (run_id, workspace_id, task, stage) = match (command.resume_run_id, probe.as_ref())
            {
                (Some(run_id), _) => {
                    let row = load_run_row(tx, run_id)?;
                    if row.status == RunStatus::Completed {
                        return Err(StoreError::RunClosed(run_id));
                    }
                    (run_id, row.workspace_id, row.task, row.stage)
                }
                // Unreachable given how `probe` is built above, but expressed as
                // an error rather than a panic: a hook must never abort a
                // session over an internal invariant.
                (None, None) => {
                    return Err(StoreError::Domain(
                        magent_core::DomainError::MissingWorkspaceRoot,
                    ));
                }
                (None, Some(probe)) => {
                    let workspace_id = upsert_repository(tx, probe, &now)?.workspace_id;
                    let run_id = RunId::new();
                    tx.execute(
                        "INSERT INTO runs (id, workspace_id, task, status, stage, created_at, updated_at)
                         VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?5)",
                        (
                            run_id.to_string(),
                            workspace_id.to_string(),
                            &command.task,
                            enum_to_sql(&WorkflowStage::Discovering)?,
                            &now,
                        ),
                    )?;
                    (
                        run_id,
                        workspace_id,
                        command.task.clone(),
                        WorkflowStage::Discovering,
                    )
                }
            };

            let session_id = SessionId::new();
            tx.execute(
                "INSERT INTO sessions (id, run_id, harness, external_session_hint, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    session_id.to_string(),
                    run_id.to_string(),
                    enum_to_sql(&harness)?,
                    command.external_session_hint.as_deref(),
                    &now,
                ),
            )?;

            Ok(StartRunResult {
                run_id,
                session_id,
                workspace_id,
                task,
                stage,
                latest_checkpoint: latest_checkpoint(tx, run_id, Some(session_id))?,
                instructions: Vec::new(),
            })
        })
    }

    /// Persists a checkpoint and advances the run's stage.
    ///
    /// A `binding` on the command is applied here, in the checkpoint's own
    /// transaction and before its `task_done` is resolved, so one message can
    /// bind a run and close its first task. It binds exactly as
    /// [`Store::bind_spec`] does — a field it does not name is left as it is.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::RunClosed`] for a completed run and
    /// [`StoreError::SessionNotFound`] for an unknown session. A `task_done`
    /// that reaches no task of the run's plan is refused by `close_task`, one
    /// variant per way it can miss, as is one that reaches its task and names a
    /// command the plan did not — the only refusal a correctly numbered tick can
    /// still meet. Either takes the whole checkpoint with it.
    pub fn save_checkpoint(
        &self,
        command: &CheckpointCommand,
    ) -> Result<CheckpointResult, StoreError> {
        command.validate()?;

        self.execute_operation("checkpoint", command.operation_id, command, |tx| {
            let run = load_run_row(tx, command.run_id)?;
            if run.status == RunStatus::Completed {
                return Err(StoreError::RunClosed(command.run_id));
            }
            assert_session_exists(tx, command.session_id)?;

            let now = Utc::now().to_rfc3339();
            let checkpoint_id = CheckpointId::new();

            tx.execute(
                "INSERT INTO checkpoints
                   (id, run_id, session_id, operation_id, stage, origin, payload_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    checkpoint_id.to_string(),
                    command.run_id.to_string(),
                    command.session_id.to_string(),
                    command.operation_id.to_string(),
                    enum_to_sql(&command.stage)?,
                    enum_to_sql(&command.origin)?,
                    serde_json::to_string(command)?,
                    &now,
                ),
            )?;

            tx.execute(
                "UPDATE runs SET stage = ?1, updated_at = ?2 WHERE id = ?3",
                (
                    enum_to_sql(&command.stage)?,
                    &now,
                    command.run_id.to_string(),
                ),
            )?;

            // In this transaction rather than in a call after it, for the same
            // reason as the tick below: a checkpoint recorded whose binding was
            // lost leaves the run pointing where it already was, while the
            // checkpoint that said otherwise is on the record.
            //
            // Before the tick, and the row re-read afterwards, because
            // `close_task` resolves the change through the run's own binding: a
            // single message that binds the run and ticks its first task, which
            // is exactly what the first checkpoint of a task looks like, would
            // otherwise be refused for a binding this very call supplied. Read
            // back rather than patched in memory, so that `write_binding`'s
            // `COALESCE` stays the only statement of what a binding leaves
            // alone.
            let run = match command.binding.as_ref() {
                Some(binding) => {
                    write_binding(tx, command.run_id, binding, &now)?;
                    load_run_row(tx, command.run_id)?
                }
                None => run,
            };

            // In this transaction rather than in a call after it: a checkpoint
            // recorded whose tick was lost is a plan that has the evidence of
            // finished work and no sign the work finished.
            let task = command
                .task_done
                .as_ref()
                .map(|done| close_task(tx, command.run_id, &run, done, &now))
                .transpose()?;

            Ok(CheckpointResult {
                checkpoint_id,
                run_id: command.run_id,
                stage: command.stage,
                task,
            })
        })
    }

    /// Closes a session, or completes the whole run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::RunClosed`] when completing an already completed
    /// run under a new operation id.
    pub fn finish_run(&self, command: &FinishRunCommand) -> Result<FinishRunResult, StoreError> {
        command.validate()?;

        self.execute_operation("finish", command.operation_id, command, |tx| {
            let run = load_run_row(tx, command.run_id)?;
            if run.status == RunStatus::Completed {
                return Err(StoreError::RunClosed(command.run_id));
            }
            assert_session_exists(tx, command.session_id)?;

            let now = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE sessions SET ended_at = ?1 WHERE id = ?2 AND ended_at IS NULL",
                (&now, command.session_id.to_string()),
            )?;

            let status = match command.action {
                FinishAction::CloseSession => RunStatus::Open,
                FinishAction::CompleteRun => {
                    tx.execute(
                        "UPDATE runs SET status = 'completed', stage = ?1, outcome = ?2, updated_at = ?3
                         WHERE id = ?4",
                        (
                            enum_to_sql(&WorkflowStage::Completed)?,
                            &command.outcome,
                            &now,
                            command.run_id.to_string(),
                        ),
                    )?;
                    RunStatus::Completed
                }
            };

            Ok(FinishRunResult {
                run_id: command.run_id,
                session_id: command.session_id,
                status,
                session_closed: true,
                outcome: command.outcome.clone(),
            })
        })
    }

    /// # Errors
    /// Returns [`StoreError::RunNotFound`] for an unknown run.
    pub fn get_run(&self, run_id: RunId) -> Result<RunSnapshot, StoreError> {
        self.snapshot_for_session(run_id, None)
    }

    /// The run as one session should see it: carrying that session's own latest
    /// checkpoint rather than whichever landed last on the run.
    ///
    /// Two agents sharing a run each read their own task back after a
    /// compaction. A session that has written none of its own falls back to the
    /// run's latest, which is what a session joining work in flight needs.
    ///
    /// # Errors
    /// Returns [`StoreError::RunNotFound`] for an unknown run.
    pub fn snapshot_for_session(
        &self,
        run_id: RunId,
        session_id: Option<SessionId>,
    ) -> Result<RunSnapshot, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let row = load_run_row(&tx, run_id)?;
        let latest_checkpoint = latest_checkpoint(&tx, run_id, session_id)?;
        drop(tx);

        Ok(RunSnapshot {
            run_id,
            workspace_id: row.workspace_id,
            task: row.task,
            status: row.status,
            stage: row.stage,
            latest_checkpoint,
            spec: row.spec,
        })
    }

    /// Points a run at the spec change it is executing.
    ///
    /// Every field is optional and `None` leaves what is stored alone, so
    /// advancing to the next task is a one-field call. Nothing here checks that
    /// the paths exist: the spec lives in git, and a run bound to a file on a
    /// branch this checkout does not have is still correctly bound. Refusing it
    /// would make the reference useless exactly where it is most wanted.
    ///
    /// This is the binding on its own, in a transaction of its own. A checkpoint
    /// that carries one writes it through the same `write_binding`, inside the
    /// checkpoint's transaction — see [`Store::save_checkpoint`].
    ///
    /// # Errors
    /// Fails on a database error, or if the run does not exist.
    pub fn bind_spec(&self, run_id: RunId, binding: &SpecBinding) -> Result<(), StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_binding(&tx, run_id, binding, &Utc::now().to_rfc3339())?;
        tx.commit()?;
        Ok(())
    }

    /// Resolves a working directory to its repository and workspace, creating
    /// them on first sight.
    ///
    /// Called from `SessionStart` on every session, so it must succeed for any
    /// directory — including one outside git entirely.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn resolve_workspace_for(&self, path: &Path) -> Result<WorkspaceResolution, StoreError> {
        // Outside the transaction: see the note in `start_run`.
        let probe = git::discover(path);

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let resolved = upsert_repository(&tx, &probe, &Utc::now().to_rfc3339())?;
        tx.commit()?;

        Ok(resolved)
    }

    // --- file ledger -------------------------------------------------------

    /// Records one observed mutation. Called from `PostToolUse`, so it must stay
    /// a single cheap insert.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn append_ledger(
        &self,
        run_id: RunId,
        session_id: SessionId,
        entry: &FileLedgerEntry,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let observed_at = entry.observed_at.to_rfc3339();

        // The task this session is holding at the moment of the edit. Stamped
        // here rather than resolved when the ledger is read, because a session
        // works several tasks in one run and 0017 keeps only the current hold:
        // by the time anyone asks, the answer is gone.
        //
        // Ordered by lease, newest first, because a session can hold more than
        // one task at once. Taking a hold on task 4 does not release task 3 —
        // `write_binding` only touches the row it names — so an agent that
        // moves on without closing leaves the old claim live for the rest of
        // its ten minutes. The newest lease is the claim most recently taken
        // or renewed, which is the task actually in hand; without the order
        // SQLite returns whichever row it scans first, and the edit is filed
        // under the task that was abandoned.
        //
        // A lapsed lease is not a hold, for the same reason the ready set
        // offers that task to somebody else. The comparison is lexicographic
        // on RFC 3339 in UTC, as `claim_job` already compares leases.
        //
        // The SELECT and the INSERT share one Immediate transaction, the
        // pattern `claim_job` and `resolve_workspace_for` already use.
        // `self.lock()` only excludes other callers in this process; a
        // concurrent replan in a second process (`DELETE FROM tasks WHERE
        // change_id = ?1`, `sdd.rs`) between the two statements would either
        // stamp a task that no longer exists, or — since `task_id` references
        // `tasks(id)` with foreign keys on — fail the INSERT outright, and the
        // hook that calls this swallows any error, so the edit would vanish
        // from the ledger rather than being misattributed. Immediate takes
        // the write lock up front, so a concurrent replan blocks on the busy
        // timeout instead of interleaving.
        let held: Option<(String, String)> = tx
            .query_row(
                "SELECT id, change_id FROM tasks
                 WHERE claimed_by = ?1 AND lease_until IS NOT NULL AND lease_until > ?2
                 ORDER BY lease_until DESC
                 LIMIT 1",
                rusqlite::params![session_id.to_string(), &observed_at],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        // Whose file this landed on, asked only when the editing session holds
        // something: with no task to file the edit under there is nothing for a
        // later close to refuse, and the row would carry an accusation nobody
        // reads. Scoped to the same change, because `files` is a contract
        // between the tasks of one plan.
        //
        // Ordered newest lease first, and the loop below stops at the first
        // match, for the same reason the hold lookup above is ordered: two
        // other sessions can each hold a different task that both declare the
        // edited file — `ready_of` (`sdd.rs`) only hides a task that is
        // itself held, not one whose files overlap another live hold, and
        // `write_binding` claims whatever `current_task` names without
        // checking for that overlap — so without an order the answer is
        // whichever row SQLite happens to scan first. The freshest hold is
        // the one actually in force, and that task number is what a later
        // refusal quotes to a person as who to go and talk to.
        let trespass_on = match &held {
            Some((_, change_id)) => {
                let mut statement = tx.prepare(
                    "SELECT number, files_json FROM tasks
                     WHERE change_id = ?1 AND claimed_by IS NOT NULL AND claimed_by != ?2
                       AND lease_until IS NOT NULL AND lease_until > ?3
                     ORDER BY lease_until DESC",
                )?;
                let rows = statement.query_map(
                    rusqlite::params![change_id, session_id.to_string(), &observed_at],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?;

                let edited = entry.path.to_string_lossy();
                let mut found = None;
                for row in rows {
                    let (number, files_json) = row?;
                    let declared: Vec<String> = serde_json::from_str(&files_json)?;
                    if declared.iter().any(|path| declares_path(path, &edited)) {
                        found = Some(number);
                        break;
                    }
                }
                found
            }
            None => None,
        };

        tx.execute(
            "INSERT INTO file_ledger (run_id, session_id, path, tool, observed_at, task_id, trespass_on)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id.to_string(),
                session_id.to_string(),
                entry.path.to_string_lossy().to_string(),
                &entry.tool,
                &observed_at,
                held.as_ref().map(|(id, _)| id),
                &trespass_on,
            ),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Distinct paths touched during a run, most recent first.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn ledger(&self, run_id: RunId, limit: usize) -> Result<Vec<FileLedgerEntry>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT path, tool, MAX(observed_at) AS observed_at
             FROM file_ledger WHERE run_id = ?1
             GROUP BY path, tool
             ORDER BY observed_at DESC
             LIMIT ?2",
        )?;

        let rows = statement.query_map(
            rusqlite::params![run_id.to_string(), i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;

        let mut entries = Vec::new();
        for row in rows {
            let (path, tool, observed_at) = row?;
            entries.push(FileLedgerEntry {
                path: PathBuf::from(path),
                tool,
                observed_at: parse_timestamp(&observed_at)?,
            });
        }
        Ok(entries)
    }

    // --- job queue ---------------------------------------------------------

    /// Enqueues background work, deduplicating on `(kind, job_key)`.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn enqueue_job(
        &self,
        kind: &str,
        job_key: &str,
        payload_json: &str,
    ) -> Result<(), StoreError> {
        let connection = self.lock()?;
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO jobs (kind, job_key, status, payload_json, created_at, updated_at)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?4)
             ON CONFLICT (kind, job_key) DO NOTHING",
            (kind, job_key, payload_json, &now),
        )?;
        Ok(())
    }

    /// Claims one runnable job of `kind`, leasing it for `lease`.
    ///
    /// A job whose lease has expired is reclaimable: a worker that died mid-job
    /// must not park the work forever.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn claim_job(&self, kind: &str, lease: Duration) -> Result<Option<Job>, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let now = Utc::now();
        let now_text = now.to_rfc3339();

        let candidate = tx
            .query_row(
                "SELECT job_key, payload_json, retry_remaining FROM jobs
                 WHERE kind = ?1
                   AND status IN ('pending', 'running')
                   AND (status = 'pending' OR (lease_until IS NOT NULL AND lease_until < ?2))
                   AND (retry_at IS NULL OR retry_at <= ?2)
                 ORDER BY created_at, job_key
                 LIMIT 1",
                (kind, &now_text),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;

        let Some((job_key, payload_json, retry_remaining)) = candidate else {
            drop(tx);
            return Ok(None);
        };

        let lease_until = (now
            + chrono::Duration::from_std(lease)
                .map_err(|error| StoreError::Database(error.to_string()))?)
        .to_rfc3339();

        tx.execute(
            "UPDATE jobs SET status = 'running', lease_until = ?1, worker_id = ?2, updated_at = ?3
             WHERE kind = ?4 AND job_key = ?5",
            (
                &lease_until,
                std::process::id().to_string(),
                &now_text,
                kind,
                &job_key,
            ),
        )?;
        tx.commit()?;

        Ok(Some(Job {
            kind: kind.to_owned(),
            job_key,
            payload_json,
            retry_remaining,
        }))
    }

    /// Current state of a queued job, for diagnosis and for tests.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn job_state(&self, kind: &str, job_key: &str) -> Result<Option<JobState>, StoreError> {
        let connection = self.lock()?;
        Ok(connection
            .query_row(
                "SELECT status, retry_remaining, last_error FROM jobs
                 WHERE kind = ?1 AND job_key = ?2",
                (kind, job_key),
                |row| {
                    Ok(JobState {
                        status: row.get(0)?,
                        retry_remaining: row.get(1)?,
                        last_error: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Records a failed attempt, scheduling a retry or giving up.
    ///
    /// A job that can never succeed must stop being handed out: without a floor
    /// it would be retried, and paid for, for as long as the database exists.
    /// The reason is kept either way, because a job that failed silently is
    /// indistinguishable from one that never ran.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn fail_job(
        &self,
        kind: &str,
        job_key: &str,
        error: &str,
        backoff: Duration,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let remaining: Option<i64> = tx
            .query_row(
                "SELECT retry_remaining FROM jobs WHERE kind = ?1 AND job_key = ?2",
                (kind, job_key),
                |row| row.get(0),
            )
            .optional()?;

        let Some(remaining) = remaining else {
            drop(tx);
            return Ok(());
        };

        let now = Utc::now();
        let left = remaining.saturating_sub(1);

        if left > 0 {
            let retry_at = now
                + chrono::Duration::from_std(backoff)
                    .map_err(|error| StoreError::Database(error.to_string()))?;
            tx.execute(
                "UPDATE jobs SET status = 'pending', retry_remaining = ?1, retry_at = ?2,
                                 lease_until = NULL, worker_id = NULL, last_error = ?3,
                                 updated_at = ?4
                 WHERE kind = ?5 AND job_key = ?6",
                (
                    left,
                    retry_at.to_rfc3339(),
                    error,
                    now.to_rfc3339(),
                    kind,
                    job_key,
                ),
            )?;
        } else {
            tx.execute(
                "UPDATE jobs SET status = 'failed', retry_remaining = 0, retry_at = NULL,
                                 lease_until = NULL, worker_id = NULL, last_error = ?1,
                                 updated_at = ?2
                 WHERE kind = ?3 AND job_key = ?4",
                (error, now.to_rfc3339(), kind, job_key),
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// # Errors
    /// Fails on a database error.
    pub fn complete_job(&self, kind: &str, job_key: &str) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE jobs SET status = 'done', lease_until = NULL, updated_at = ?1
             WHERE kind = ?2 AND job_key = ?3",
            (Utc::now().to_rfc3339(), kind, job_key),
        )?;
        Ok(())
    }

    // --- idempotency -------------------------------------------------------

    /// Runs `body` exactly once per `operation_id`.
    ///
    /// A replay with the same request returns the recorded response. A replay
    /// carrying a *different* request is an error rather than a silent answer to
    /// a question that was never asked.
    pub(crate) fn execute_operation<C, R>(
        &self,
        kind: &str,
        operation_id: OperationId,
        command: &C,
        body: impl FnOnce(&Transaction<'_>) -> Result<R, StoreError>,
    ) -> Result<R, StoreError>
    where
        C: Serialize,
        R: Serialize + DeserializeOwned,
    {
        let request_json = serde_json::to_string(command)?;

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(recorded) = lookup_operation(&tx, operation_id, kind, &request_json)? {
            drop(tx);
            return Ok(serde_json::from_str(&recorded)?);
        }

        let result = body(&tx)?;

        tx.execute(
            "INSERT INTO operations (operation_id, operation_kind, request_json, response_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                operation_id.to_string(),
                kind,
                &request_json,
                serde_json::to_string(&result)?,
                Utc::now().to_rfc3339(),
            ),
        )?;
        tx.commit()?;

        Ok(result)
    }
}

// --- free helpers ----------------------------------------------------------

/// Puts the database into WAL mode, tolerating a concurrent opener.
///
/// Switching journal mode takes an exclusive lock, and `SQLite` does *not* run the
/// busy handler for it: a competing `Store::open` gets `SQLITE_BUSY` back
/// immediately rather than waiting out `busy_timeout`. Since every hook
/// invocation opens the store, two of them starting together would otherwise
/// race and one would fail outright.
///
/// Re-asserting WAL on a database that is already in WAL still takes that lock,
/// so the mode is read first and written only when it actually has to change —
/// which makes the contended path the rare one-time case of first creation.
fn ensure_wal(connection: &Connection) -> Result<(), StoreError> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;

    loop {
        let current: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        if current.eq_ignore_ascii_case("wal") {
            return Ok(());
        }

        let outcome = connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        });

        match outcome {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) if std::time::Instant::now() >= deadline => {
                return Err(StoreError::Database(format!(
                    "database stayed in {mode} mode; WAL is required"
                )));
            }
            Err(error) if std::time::Instant::now() >= deadline => {
                return Err(StoreError::Database(error.to_string()));
            }
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub(crate) struct RunRow {
    workspace_id: WorkspaceId,
    task: String,
    status: RunStatus,
    stage: WorkflowStage,
    spec: Option<SpecBinding>,
}

pub(crate) fn load_run_row(tx: &Transaction<'_>, run_id: RunId) -> Result<RunRow, StoreError> {
    let row = tx
        .query_row(
            "SELECT workspace_id, task, status, stage, spec_change_id, current_task
             FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::RunNotFound(run_id))?;

    // A binding exists only when something is in it. An empty one reads as a
    // broken reference rather than as the absence of one, and plenty of work is
    // not spec-driven.
    let bound = row.4.is_some() || row.5.is_some();
    let spec = bound.then_some(SpecBinding {
        change_id: row.4,
        current_task: row.5,
    });

    Ok(RunRow {
        workspace_id: parse_id(&row.0)?,
        task: row.1,
        status: enum_from_sql(&row.2)?,
        stage: enum_from_sql(&row.3)?,
        spec,
    })
}

/// Writes a binding onto a run, leaving alone whatever it does not name.
///
/// Takes the transaction rather than opening one, because the two callers differ
/// in exactly that: [`Store::bind_spec`] binds a run on its own, while a
/// checkpoint's binding has to land in the checkpoint's own transaction or it is
/// How long a hold on a task lasts before it lapses.
///
/// Renewed by every checkpoint, so an agent that keeps reporting keeps its
/// task, and one that goes quiet for longer than this is indistinguishable
/// from one that stopped. Ten minutes: long enough for a slow build between
/// checkpoints, short enough that a dead agent does not park the plan.
const TASK_LEASE: chrono::Duration = chrono::Duration::minutes(10);

/// The task number a `current_task` names, if it names one.
///
/// `current_task` is free text — `sdd-execute` writes `2: wire the budget`, and
/// a checkpoint for work that is not a task at all writes whatever describes
/// it. So this takes the leading token and hands it back only as a candidate;
/// whether a task of that number exists is the caller's question, and a
/// checkpoint naming none claims nothing and is not an error.
fn task_number_in(current_task: &str) -> Option<&str> {
    let head = current_task
        .trim()
        .split(|character: char| character.is_whitespace() || character == ':')
        .next()?
        .trim_end_matches('.');

    (!head.is_empty()
        && head
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.'))
    .then_some(head)
}

/// Whether a path a plan declared names the file an edit landed on.
///
/// A plan writes repository-relative paths (`src/store.rs`); the hook records
/// what the harness handed it, which is absolute. Nothing here can bridge the
/// two by construction — `repositories.canonical_root` is where the repository
/// was first seen, and a worktree of it resolves somewhere else — so the match
/// is made on the tail, at a separator boundary. `src/store.rs` matches
/// `/home/me/project/src/store.rs` and not `/home/me/project/src/store.rs.bak`,
/// nor `/home/me/other_src/store.rs` — a different directory of the same
/// leaf name is not a match either. A plan declaring the bare name
/// `store.rs` matches any file so called, which is what declaring a bare
/// name asks for. A leading `./` is trimmed, so `./src/store.rs` behaves
/// exactly like `src/store.rs`. An empty declaration — `""`, or all
/// whitespace — matches nothing, deliberately: `format!("/{declared}")` on
/// an empty string is `/`, which `ends_with` would only match against an
/// edited path ending in a separator — a directory, which the hook never
/// reports — so the guard closes that off outright rather than leaning on
/// the hook never sending one.
fn declares_path(declared: &str, edited: &str) -> bool {
    let declared = declared.trim().trim_start_matches("./");

    !declared.is_empty() && (edited == declared || edited.ends_with(&format!("/{declared}")))
}

/// a write that can be lost while the checkpoint survives.
fn write_binding(
    tx: &Transaction<'_>,
    run_id: RunId,
    binding: &SpecBinding,
    now: &str,
) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE runs SET
             spec_change_id = COALESCE(?1, spec_change_id),
             current_task   = COALESCE(?2, current_task),
             updated_at     = ?3
         WHERE id = ?4",
        (
            &binding.change_id,
            &binding.current_task,
            now,
            run_id.to_string(),
        ),
    )?;

    if changed == 0 {
        return Err(StoreError::RunNotFound(run_id));
    }

    // The claim rides on the binding rather than on a verb of its own: this is
    // already written before the work starts, and a second way to say the same
    // thing would be a second thing to forget. Best effort by design — a
    // checkpoint whose `current_task` names no task of the plan claims nothing
    // and is not refused, because the field carries work that is not a task.
    if let Some(current_task) = binding.current_task.as_deref()
        && let Some(number) = task_number_in(current_task)
    {
        let until = (Utc::now() + TASK_LEASE).to_rfc3339();
        tx.execute(
            "UPDATE tasks
                SET claimed_by = (SELECT id FROM sessions
                                  WHERE run_id = ?1 ORDER BY last_seen_at DESC LIMIT 1),
                    lease_until = ?2,
                    status = 'running',
                    updated_at = ?3
              WHERE number = ?4
                AND status IN ('pending', 'running')
                AND change_id IN (SELECT c.id FROM sdd_changes c
                                  JOIN runs r ON r.spec_change_id = c.slug
                                  WHERE r.id = ?1)",
            rusqlite::params![run_id.to_string(), &until, now, number],
        )?;
    }

    Ok(())
}

/// Closes the task a checkpoint ticks off, and says what the tick did.
///
/// The change is taken from the run's own binding rather than from the caller:
/// `runs.spec_change_id` holds a slug (`0001_slice1.sql`), and a checkpoint late
/// in a task carries the tick and nothing else, so a tick that had to restate
/// its change would be one an agent could get wrong.
///
/// `evidence` and `verified_at` are written with the status, which is what
/// `0009_tasks.sql` means by their landing together: a task recorded as done
/// with no record of what proved it reads afterwards exactly like one that was
/// checked. The output is stored as it came — trimming or summarising it would
/// edit the one part of a tick a later reader can judge for themselves — and
/// `expected_output` is compared only to report the comparison, because the
/// plan wrote those markers before the work was done and refusing a tick over
/// them would stop correct work. What comes back is the markers the output does
/// not carry, not a verdict on the pair: a reader who can see which marker went
/// missing can tell a renamed test from a run that genuinely failed.
///
/// A tick that leaves no task open moves the change to `ready` in the same
/// transaction: that is what `0009_tasks.sql` means by a change reaching `ready`
/// when its tasks are all done, and it is what [`Store::archive`] waits for.
///
/// Every refusal below comes before the `UPDATE`, and they come in this order:
/// the run's binding, then what the slug resolves to, then the number, then the
/// command, then the ledger. Cheapest and most fundamental first, so a caller
/// whose run is unbound is told that rather than told its number is unknown —
/// which would be true, and would send it to fix the wrong thing. The ledger
/// check comes last because it is the only one that reads another table.
fn close_task(
    tx: &Transaction<'_>,
    run_id: RunId,
    run: &RunRow,
    done: &TaskDone,
    now: &str,
) -> Result<TaskClosed, StoreError> {
    let slug = run
        .spec
        .as_ref()
        .and_then(|spec| spec.change_id.as_deref())
        .ok_or(StoreError::RunNotBoundToChange { run: run_id })?;

    let mut found = sdd::change_by_slug(tx, &run.workspace_id.to_string(), slug)?;
    if found.len() > 1 {
        return Err(StoreError::ChangeSlugAmbiguous {
            slug: slug.to_owned(),
            namespaces: found
                .into_iter()
                // Named rather than left blank: a change filed under no
                // namespace is one of the two the caller has to tell apart, and
                // an empty entry in that list reads as a formatting fault.
                .map(|(_, namespace)| namespace.unwrap_or_else(|| "(no namespace)".to_owned()))
                .collect(),
        });
    }
    let (change_id, _) = found
        .pop()
        .ok_or_else(|| StoreError::ChangeSlugNotFound(slug.to_owned()))?;

    let planned: Option<(String, String)> = tx
        .query_row(
            "SELECT verify_command, expected_output_json FROM tasks
             WHERE change_id = ?1 AND number = ?2",
            rusqlite::params![&change_id, &done.number],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((verify_command, expected_output_json)) = planned else {
        return Err(StoreError::TaskNotFound {
            slug: slug.to_owned(),
            number: done.number.clone(),
            open: sdd::open_task_numbers(tx, &change_id)?,
        });
    };

    // Trimmed on both sides and then exact. This is the check that makes "run
    // the command the plan stated" a rule rather than a suggestion, and a looser
    // comparison would file the output of some neighbouring command as the proof
    // this task was done. Whitespace around either is how the string was written
    // and not part of what it says, which is the one difference worth forgiving.
    if verify_command.trim() != done.verify_command.trim() {
        return Err(StoreError::VerifyCommandMismatch {
            number: done.number.clone(),
            expected: verify_command,
        });
    }

    // Read against what was recorded, not against what the closer reports —
    // the same principle as the command check above. The earliest row wins:
    // one collision is enough to stop the tick, and naming the first keeps the
    // message about a file rather than a list.
    let trespass: Option<(String, String)> = tx
        .query_row(
            "SELECT l.path, l.trespass_on FROM file_ledger l
             JOIN tasks t ON t.id = l.task_id
             WHERE t.change_id = ?1 AND t.number = ?2 AND l.trespass_on IS NOT NULL
             ORDER BY l.id LIMIT 1",
            rusqlite::params![&change_id, &done.number],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((path, holder)) = trespass {
        return Err(StoreError::FileHeldByAnotherTask {
            number: done.number.clone(),
            path,
            holder,
        });
    }

    // Trimmed on the plan's side only. A plan states the markers it expects to
    // see, and the whitespace around each is how the plan was written rather
    // than part of what it claims; the output keeps every character of its own,
    // being quoted evidence.
    let markers: Vec<String> = serde_json::from_str(&expected_output_json)?;
    let expected_output_missing: Vec<String> = markers
        .into_iter()
        .filter(|marker| !done.output.contains(marker.trim()))
        .collect();

    tx.execute(
        // The hold goes with the status: a done task holds nothing.
        "UPDATE tasks SET status = 'done', evidence = ?1, verified_at = ?2, updated_at = ?3,
                          claimed_by = NULL, lease_until = NULL
         WHERE change_id = ?4 AND number = ?5",
        rusqlite::params![&done.output, now, now, &change_id, &done.number],
    )?;

    // The same tick again, where a replan cannot reach it. `Store::plan`
    // deletes the change's tasks, so the `evidence` written just above lives on
    // a row the next plan of this change is entitled to remove — see
    // `0012_task_ticks.sql`. In this transaction with the `UPDATE`, which is
    // the argument above one table further: a task recorded as done whose tick
    // was lost reads afterwards exactly like one that was never checked.
    tx.execute(
        "INSERT INTO task_ticks
           (id, change_id, number, verify_command, output, missing_json, run_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            uuid::Uuid::new_v4().to_string(),
            &change_id,
            &done.number,
            &done.verify_command,
            &done.output,
            serde_json::to_string(&expected_output_missing)?,
            run_id.to_string(),
            now,
        ],
    )?;

    let change_ready = mark_change_ready(tx, &change_id, now)?;

    Ok(TaskClosed {
        number: done.number.clone(),
        expected_output_missing,
        change_ready,
    })
}

/// Moves a change to `ready` once no task of its plan is open, and says whether
/// it is there. The only place that writes the status `0009_tasks.sql` promises
/// a change reaches when its tasks are all done.
///
/// Readiness is asked of [`sdd::open_task_numbers`], which is the definition
/// `require_tasks_closed` archives by. A second reading of "open" written here
/// could report a change ready that archiving then refuses, and a caller told
/// both has no way out.
///
/// No predicate on the change's own status, which is worth stating because two
/// statuses reach here differently. An archived or abandoned change cannot:
/// `close_task` resolved this id through `change_by_slug`, out of the live set,
/// so it was refused earlier as a slug nothing answers to — the same refusal
/// `require_archivable_change` words for the archive side. A `specified` change
/// can, and deliberately gets no guard: `specify` pulls a planned change back
/// without deleting its tasks, so ticking the rest of an old plan lands here on
/// a change whose newest delta no task covers, and it reads `ready`. Guarding
/// the row would leave the returned flag — computed from the tasks — saying
/// ready while the row said otherwise, and a caller told both has no way out.
///
/// `ready` is therefore a status a change can reach with a requirement nobody
/// planned for, and that is caught where it matters: `require_covered_by_done`
/// refuses to archive one. The way out is a replan, which `ready` now allows —
/// it did not, until the tick journal made replanning lose nothing.
fn mark_change_ready(tx: &Transaction<'_>, change_id: &str, now: &str) -> Result<bool, StoreError> {
    if !sdd::open_task_numbers(tx, change_id)?.is_empty() {
        return Ok(false);
    }

    tx.execute(
        "UPDATE sdd_changes SET status = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![enum_to_sql(&ChangeStatus::Ready)?, now, change_id],
    )?;

    Ok(true)
}

fn assert_session_exists(tx: &Transaction<'_>, session_id: SessionId) -> Result<(), StoreError> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
        [session_id.to_string()],
        |row| row.get(0),
    )?;

    if exists {
        Ok(())
    } else {
        Err(StoreError::SessionNotFound(session_id))
    }
}

/// Most recent checkpoint for a run.
///
/// The checkpoint a session should be shown, which is its own latest.
///
/// Falls back to the run's latest when this session has written none. That is a
/// trade taken deliberately: a session joining work already in flight has
/// nothing of its own and needs to know where things stand, and handing it
/// nothing would be worse than handing it a neighbour's. The cost is that two
/// agents see each other's task until each has checkpointed once.
///
/// Ordered by rowid, not `created_at`: several checkpoints can land in the same
/// millisecond, and "latest" must never be ambiguous.
fn latest_checkpoint(
    tx: &Transaction<'_>,
    run_id: RunId,
    session_id: Option<SessionId>,
) -> Result<Option<CheckpointSnapshot>, StoreError> {
    if let Some(session_id) = session_id
        && let Some(own) = checkpoint_row(
            tx,
            run_id,
            "SELECT id, session_id, origin, payload_json, created_at
             FROM checkpoints WHERE run_id = ?1 AND session_id = ?2
             ORDER BY rowid DESC LIMIT 1",
            rusqlite::params![run_id.to_string(), session_id.to_string()],
        )?
    {
        return Ok(Some(own));
    }

    checkpoint_row(
        tx,
        run_id,
        "SELECT id, session_id, origin, payload_json, created_at
         FROM checkpoints WHERE run_id = ?1
         ORDER BY rowid DESC LIMIT 1",
        rusqlite::params![run_id.to_string()],
    )
}

/// The half both queries above share: one row, decoded into a snapshot.
fn checkpoint_row(
    tx: &Transaction<'_>,
    run_id: RunId,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Option<CheckpointSnapshot>, StoreError> {
    let row = tx
        .query_row(sql, params, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .optional()?;

    let Some((id, session_id, origin, payload_json, created_at)) = row else {
        return Ok(None);
    };

    let command: CheckpointCommand = serde_json::from_str(&payload_json)?;
    let origin: CheckpointOrigin = enum_from_sql(&origin)?;

    Ok(Some(CheckpointSnapshot {
        checkpoint_id: parse_id(&id)?,
        run_id,
        session_id: parse_id(&session_id)?,
        stage: command.stage,
        origin,
        completed_steps: command.completed_steps,
        next_steps: command.next_steps,
        decisions: command.decisions,
        rejected: command.rejected,
        changed_files: command.changed_files,
        verification: command.verification,
        risks: command.risks,
        handoff_summary: command.handoff_summary,
        created_at: parse_timestamp(&created_at)?,
    }))
}

/// Records a probed repository, creating it and a single-repository workspace
/// when its identity is unseen.
///
/// Grouping several repositories into one workspace stays an explicit action:
/// guessing from directory layout is wrong often enough (`opensource/forks`,
/// scratch clones) that a wrong guess would silently merge unrelated memories.
pub(crate) fn upsert_repository(
    tx: &Transaction<'_>,
    probe: &RepositoryProbe,
    now: &str,
) -> Result<WorkspaceResolution, StoreError> {
    let identity_key = probe.identity_key();
    let root_text = probe.root.to_string_lossy().to_string();

    let existing = tx
        .query_row(
            "SELECT id, workspace_id, canonical_root, role FROM repositories
             WHERE identity_key = ?1",
            [&identity_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    if let Some((repository_id, workspace_id, canonical_root, role)) = existing {
        return Ok(WorkspaceResolution {
            workspace_id: parse_id(&workspace_id)?,
            repository_id: parse_id(&repository_id)?,
            identity_key,
            toplevel: PathBuf::from(canonical_root),
            origin_url: probe.origin_url.clone(),
            git: probe.git.clone(),
            role: enum_from_sql(&role)?,
        });
    }

    // A repository first seen while git was unavailable was filed under its
    // path, because that was all that could be known. When git returns, the same
    // directory resolves to an origin — and a second row for it would split the
    // project's memory in two, silently and for good. The existing row is
    // upgraded instead.
    //
    // Only ever path to origin: an origin identity is the better one, and
    // downgrading would undo the merge on the next outage.
    if probe.origin_url.is_some()
        && let Some((repository_id, workspace_id, role)) = tx
            .query_row(
                "SELECT id, workspace_id, role FROM repositories
                 WHERE canonical_root = ?1 AND identity_key LIKE 'path:%'",
                [&root_text],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
    {
        tx.execute(
            "UPDATE repositories SET identity_key = ?1, origin_url = ?2 WHERE id = ?3",
            (&identity_key, probe.origin_url.as_deref(), &repository_id),
        )?;

        return Ok(WorkspaceResolution {
            workspace_id: parse_id(&workspace_id)?,
            repository_id: parse_id(&repository_id)?,
            identity_key,
            toplevel: probe.root.clone(),
            origin_url: probe.origin_url.clone(),
            git: probe.git.clone(),
            role: enum_from_sql(&role)?,
        });
    }

    let workspace_id = WorkspaceId::new();
    let repository_id = RepositoryId::new();
    let name = probe
        .root
        .file_name()
        .map_or_else(|| root_text.clone(), |n| n.to_string_lossy().to_string());

    tx.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
        (workspace_id.to_string(), &name, now),
    )?;
    tx.execute(
        "INSERT INTO repositories (id, workspace_id, identity_key, canonical_root, origin_url, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            repository_id.to_string(),
            workspace_id.to_string(),
            &identity_key,
            &root_text,
            probe.origin_url.as_deref(),
            now,
        ),
    )?;

    Ok(WorkspaceResolution {
        workspace_id,
        repository_id,
        identity_key,
        toplevel: probe.root.clone(),
        origin_url: probe.origin_url.clone(),
        git: probe.git.clone(),
        role: RepositoryRole::default(),
    })
}

fn lookup_operation(
    tx: &Transaction<'_>,
    operation_id: OperationId,
    kind: &str,
    request_json: &str,
) -> Result<Option<String>, StoreError> {
    let recorded = tx
        .query_row(
            "SELECT operation_kind, request_json, response_json FROM operations
             WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;

    match recorded {
        None => Ok(None),
        Some((recorded_kind, recorded_request, response)) => {
            if recorded_kind == kind && recorded_request == request_json {
                Ok(Some(response))
            } else {
                Err(StoreError::IdempotencyConflict(operation_id))
            }
        }
    }
}

/// Enums are stored using their wire names, so the database and the JSON
/// contract can never drift apart.
pub(crate) fn enum_to_sql<T: Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Serialization("expected a string-valued enum".into()))
}

pub(crate) fn enum_from_sql<T: DeserializeOwned>(raw: &str) -> Result<T, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        raw.to_owned(),
    ))?)
}

pub(crate) fn parse_id<T: std::str::FromStr<Err = uuid::Error>>(
    raw: &str,
) -> Result<T, StoreError> {
    raw.parse()
        .map_err(|error: uuid::Error| StoreError::Serialization(error.to_string()))
}

pub(crate) fn parse_timestamp(raw: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| StoreError::Serialization(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `declares_path` is a pure string function whose interesting cases are
    // all boundaries. Driving five path shapes through a full database
    // fixture (`store_contract.rs`) would cost ten times as much for the same
    // answer, so its behaviour is pinned here instead — the crate's one
    // deliberate exception to testing only through the public contract.

    #[test]
    fn a_declared_path_matches_only_at_a_separator_boundary() {
        assert!(
            declares_path("src/store.rs", "/home/me/project/src/store.rs"),
            "a declared relative path must match the absolute path the hook hands over"
        );
        assert!(
            !declares_path("src/store.rs", "/home/me/project/src/store.rs.bak"),
            "a longer file name sharing the same characters must not match — \
             this is the boundary a switch to `contains` would silently break"
        );
        assert!(
            !declares_path("src/store.rs", "/home/me/other_src/store.rs"),
            "the same leaf name under a different directory must not match — \
             this is the boundary a missing leading `/` in the match would silently break"
        );
    }

    #[test]
    fn a_bare_declared_name_matches_any_file_so_named() {
        assert!(
            declares_path("store.rs", "/home/me/project/src/store.rs"),
            "a plan declaring a bare file name asks to match that name anywhere"
        );
    }

    #[test]
    fn a_leading_dot_slash_is_trimmed_before_matching() {
        assert!(
            declares_path("./src/store.rs", "/home/me/project/src/store.rs"),
            "`./src/store.rs` must behave exactly like `src/store.rs`"
        );
    }

    #[test]
    fn an_empty_declaration_matches_nothing() {
        assert!(
            !declares_path("", "/home/me/project/src/store.rs"),
            "an empty declared path must match nothing — otherwise a stray blank \
             entry in `files` would make every edit in the change a trespass, \
             and no task of that plan could ever close"
        );
        assert!(
            !declares_path("   ", "/home/me/project/src/store.rs"),
            "an all-whitespace declared path must match nothing, for the same reason"
        );
    }
}
