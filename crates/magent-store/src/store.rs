use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use chrono::{DateTime, Utc};
use magent_core::{
    CheckpointCommand, CheckpointId, CheckpointOrigin, CheckpointResult, CheckpointSnapshot,
    FileLedgerEntry, FinishAction, FinishRunCommand, FinishRunResult, HarnessKind, OperationId,
    RunId, RunSnapshot, RunStatus, SessionId, StartRunCommand, StartRunResult, Validate,
    WorkflowStage, WorkspaceId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Serialize, de::DeserializeOwned};

use crate::{error::StoreError, migrations};

/// How long a writer waits for a competing writer before giving up.
///
/// With no daemon, several hook processes and the MCP server contend for the
/// same file. Five seconds is far beyond any legitimate write here, so hitting
/// it means something is wedged rather than merely busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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
    connection: Mutex<Connection>,
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

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
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

        self.execute_operation("start_run", command.operation_id, command, |tx| {
            let now = Utc::now().to_rfc3339();

            let (run_id, workspace_id, task, stage) = if let Some(run_id) = command.resume_run_id
            {
                let row = load_run_row(tx, run_id)?;
                if row.status == RunStatus::Completed {
                    return Err(StoreError::RunClosed(run_id));
                }
                (run_id, row.workspace_id, row.task, row.stage)
            } else {
                    let workspace_id = resolve_workspace(tx, &command.workspace_roots, &now)?;
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
                latest_checkpoint: latest_checkpoint(tx, run_id)?,
                instructions: Vec::new(),
            })
        })
    }

    /// Persists a checkpoint and advances the run's stage.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::RunClosed`] for a completed run and
    /// [`StoreError::SessionNotFound`] for an unknown session.
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

            Ok(CheckpointResult {
                checkpoint_id,
                run_id: command.run_id,
                stage: command.stage,
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
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let row = load_run_row(&tx, run_id)?;
        let latest_checkpoint = latest_checkpoint(&tx, run_id)?;
        drop(tx);

        Ok(RunSnapshot {
            run_id,
            workspace_id: row.workspace_id,
            task: row.task,
            status: row.status,
            stage: row.stage,
            latest_checkpoint,
        })
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
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO file_ledger (run_id, session_id, path, tool, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                run_id.to_string(),
                session_id.to_string(),
                entry.path.to_string_lossy().to_string(),
                &entry.tool,
                entry.observed_at.to_rfc3339(),
            ),
        )?;
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
    fn execute_operation<C, R>(
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

struct RunRow {
    workspace_id: WorkspaceId,
    task: String,
    status: RunStatus,
    stage: WorkflowStage,
}

fn load_run_row(tx: &Transaction<'_>, run_id: RunId) -> Result<RunRow, StoreError> {
    let row = tx
        .query_row(
            "SELECT workspace_id, task, status, stage FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::RunNotFound(run_id))?;

    Ok(RunRow {
        workspace_id: parse_id(&row.0)?,
        task: row.1,
        status: enum_from_sql(&row.2)?,
        stage: enum_from_sql(&row.3)?,
    })
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
/// Ordered by rowid, not `created_at`: several checkpoints can land in the same
/// millisecond, and "latest" must never be ambiguous.
fn latest_checkpoint(
    tx: &Transaction<'_>,
    run_id: RunId,
) -> Result<Option<CheckpointSnapshot>, StoreError> {
    let row = tx
        .query_row(
            "SELECT id, session_id, origin, payload_json, created_at
             FROM checkpoints WHERE run_id = ?1
             ORDER BY rowid DESC LIMIT 1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
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

/// Maps the first workspace root onto a repository, creating both it and a
/// single-repository workspace when unseen.
///
/// Slice 3 replaces path identity with git identity; until then a canonical
/// path is enough to keep runs from leaking between projects.
fn resolve_workspace(
    tx: &Transaction<'_>,
    roots: &[PathBuf],
    now: &str,
) -> Result<WorkspaceId, StoreError> {
    let root = roots.first().ok_or(StoreError::Domain(
        magent_core::DomainError::MissingWorkspaceRoot,
    ))?;
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
    let canonical_text = canonical.to_string_lossy().to_string();

    let existing: Option<String> = tx
        .query_row(
            "SELECT workspace_id FROM repositories WHERE canonical_root = ?1",
            [&canonical_text],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(workspace_id) = existing {
        return parse_id(&workspace_id);
    }

    let workspace_id = WorkspaceId::new();
    let name = canonical.file_name().map_or_else(
        || canonical_text.clone(),
        |n| n.to_string_lossy().to_string(),
    );

    tx.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
        (workspace_id.to_string(), &name, now),
    )?;
    tx.execute(
        "INSERT INTO repositories (id, workspace_id, canonical_root, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        (
            magent_core::RepositoryId::new().to_string(),
            workspace_id.to_string(),
            &canonical_text,
            now,
        ),
    )?;

    Ok(workspace_id)
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
fn enum_to_sql<T: Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Serialization("expected a string-valued enum".into()))
}

fn enum_from_sql<T: DeserializeOwned>(raw: &str) -> Result<T, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        raw.to_owned(),
    ))?)
}

fn parse_id<T: std::str::FromStr<Err = uuid::Error>>(raw: &str) -> Result<T, StoreError> {
    raw.parse()
        .map_err(|error: uuid::Error| StoreError::Serialization(error.to_string()))
}

fn parse_timestamp(raw: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| StoreError::Serialization(error.to_string()))
}
