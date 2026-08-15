//! Contract tests for the durable store.
//!
//! Every test directs state at a fresh temporary directory. A test that touched
//! the real `~/.magent` would corrupt working memory, so this is not optional.

use std::{path::Path, process::Command, time::Duration};

use magent_core::{
    CheckpointCommand, CheckpointOrigin, FinishAction, FinishRunCommand, HarnessKind, OperationId,
    RunId, RunStatus, SessionId, StartRunCommand, WorkflowStage,
};
use magent_store::{CURRENT_VERSION, Store, StoreError};

/// Set on a re-invocation of this test binary to make it act as a competing
/// writer process instead of running assertions.
const CHILD_DB_ENV: &str = "MAGENT_TEST_CONCURRENT_DB";
const CHILD_TAG_ENV: &str = "MAGENT_TEST_CONCURRENT_TAG";
const WRITES_PER_CHILD: usize = 25;

fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("magent.db");
    (dir, path)
}

fn start_command(task: &str) -> StartRunCommand {
    StartRunCommand {
        operation_id: OperationId::new(),
        task: task.into(),
        resume_run_id: None,
        external_session_hint: None,
        workspace_roots: vec![std::env::temp_dir()],
    }
}

fn resume_command(run_id: RunId) -> StartRunCommand {
    StartRunCommand {
        resume_run_id: Some(run_id),
        ..start_command("fix payment timeout")
    }
}

fn checkpoint_command(run_id: RunId, session_id: SessionId, summary: &str) -> CheckpointCommand {
    CheckpointCommand {
        operation_id: OperationId::new(),
        run_id,
        session_id,
        stage: WorkflowStage::Executing,
        origin: CheckpointOrigin::Deterministic,
        completed_steps: vec!["located owner".into()],
        next_steps: vec!["write regression test".into()],
        decisions: vec!["keep public API compatible".into()],
        rejected: vec![],
        changed_files: vec!["src/service.rs".into()],
        verification: vec!["targeted test is red".into()],
        risks: vec![],
        handoff_summary: summary.into(),
    }
}

// --- schema ----------------------------------------------------------------

/// WAL is what makes the daemonless design work: several short-lived hook
/// processes and the MCP server all write the same file.
#[test]
fn open_sets_the_pragmas_the_design_depends_on() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");

    assert_eq!(store.journal_mode().expect("journal_mode"), "wal");
    assert!(store.foreign_keys_enabled().expect("foreign_keys"));
    // Against the constant, not a literal: the property is that opening
    // migrates to what this build understands, which changes every slice.
    assert_eq!(
        store.schema_version().expect("schema_version"),
        CURRENT_VERSION
    );
}

#[test]
fn opening_an_existing_database_is_a_no_op() {
    let (_dir, path) = temp_db();
    Store::open(&path).expect("first open");
    let reopened = Store::open(&path).expect("second open");

    assert_eq!(
        reopened.schema_version().expect("schema_version"),
        CURRENT_VERSION
    );
}

// --- durability ------------------------------------------------------------

#[test]
fn checkpoint_survives_reopen_and_is_returned_on_resume() {
    let (_dir, path) = temp_db();

    let store = Store::open(&path).expect("open");
    let started = store
        .start_run(
            &start_command("fix payment timeout"),
            HarnessKind::ClaudeCode,
        )
        .expect("start");
    let saved = store
        .save_checkpoint(&checkpoint_command(
            started.run_id,
            started.session_id,
            "owner traced; regression test is next",
        ))
        .expect("checkpoint");
    drop(store);

    let reopened = Store::open(&path).expect("reopen");
    let resumed = reopened
        .start_run(&resume_command(started.run_id), HarnessKind::Codex)
        .expect("resume");

    assert_eq!(resumed.run_id, started.run_id, "resume keeps the run");
    assert_ne!(
        resumed.session_id, started.session_id,
        "resume opens a new session"
    );

    let checkpoint = resumed.latest_checkpoint.expect("latest checkpoint");
    assert_eq!(checkpoint.checkpoint_id, saved.checkpoint_id);
    assert_eq!(
        checkpoint.handoff_summary,
        "owner traced; regression test is next"
    );
}

#[test]
fn latest_checkpoint_is_the_most_recent_one() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");
    let started = store
        .start_run(&start_command("trace the leak"), HarnessKind::ClaudeCode)
        .expect("start");

    for summary in ["first", "second", "third"] {
        store
            .save_checkpoint(&checkpoint_command(
                started.run_id,
                started.session_id,
                summary,
            ))
            .expect("checkpoint");
    }

    let snapshot = store.get_run(started.run_id).expect("get_run");
    assert_eq!(
        snapshot
            .latest_checkpoint
            .expect("checkpoint")
            .handoff_summary,
        "third"
    );
}

// --- idempotency -----------------------------------------------------------

/// A hook that retries after a crash must not create a second checkpoint.
#[test]
fn replaying_an_operation_returns_the_original_result() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");
    let started = store
        .start_run(
            &start_command("fix payment timeout"),
            HarnessKind::ClaudeCode,
        )
        .expect("start");

    let command = checkpoint_command(started.run_id, started.session_id, "same body");
    let first = store.save_checkpoint(&command).expect("first");
    let replayed = store.save_checkpoint(&command).expect("replay");

    assert_eq!(replayed.checkpoint_id, first.checkpoint_id);
    assert_eq!(store.checkpoint_count(started.run_id).expect("count"), 1);
}

/// Reusing an id with a different body is a bug in the caller. Replaying the
/// stored response would silently hand back an answer to a different question.
#[test]
fn reusing_an_operation_id_with_a_different_body_is_rejected() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");
    let started = store
        .start_run(
            &start_command("fix payment timeout"),
            HarnessKind::ClaudeCode,
        )
        .expect("start");

    let first = checkpoint_command(started.run_id, started.session_id, "original");
    store.save_checkpoint(&first).expect("first");

    let conflicting = CheckpointCommand {
        handoff_summary: "something else entirely".into(),
        ..first.clone()
    };

    match store.save_checkpoint(&conflicting) {
        Err(StoreError::IdempotencyConflict(id)) => assert_eq!(id, first.operation_id),
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    assert_eq!(store.checkpoint_count(started.run_id).expect("count"), 1);
}

// --- run lifecycle ---------------------------------------------------------

#[test]
fn close_session_leaves_the_run_open() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");
    let started = store
        .start_run(
            &start_command("fix payment timeout"),
            HarnessKind::ClaudeCode,
        )
        .expect("start");

    let finished = store
        .finish_run(&FinishRunCommand {
            operation_id: OperationId::new(),
            run_id: started.run_id,
            session_id: started.session_id,
            action: FinishAction::CloseSession,
            outcome: "handing over".into(),
        })
        .expect("finish");

    assert_eq!(finished.status, RunStatus::Open);
    assert!(finished.session_closed);
    assert_eq!(
        store.get_run(started.run_id).expect("get_run").status,
        RunStatus::Open
    );
}

#[test]
fn completed_run_rejects_further_checkpoints_and_sessions() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");
    let started = store
        .start_run(
            &start_command("fix payment timeout"),
            HarnessKind::ClaudeCode,
        )
        .expect("start");

    store
        .finish_run(&FinishRunCommand {
            operation_id: OperationId::new(),
            run_id: started.run_id,
            session_id: started.session_id,
            action: FinishAction::CompleteRun,
            outcome: "verified".into(),
        })
        .expect("complete");

    let late = store.save_checkpoint(&checkpoint_command(
        started.run_id,
        started.session_id,
        "too late",
    ));
    assert!(
        matches!(late, Err(StoreError::RunClosed(id)) if id == started.run_id),
        "expected RunClosed, got {late:?}"
    );

    let reopen = store.start_run(&resume_command(started.run_id), HarnessKind::Codex);
    assert!(
        matches!(reopen, Err(StoreError::RunClosed(id)) if id == started.run_id),
        "expected RunClosed, got {reopen:?}"
    );
}

#[test]
fn resuming_an_unknown_run_is_rejected() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");
    let missing = RunId::new();

    let result = store.start_run(&resume_command(missing), HarnessKind::ClaudeCode);
    assert!(
        matches!(result, Err(StoreError::RunNotFound(id)) if id == missing),
        "expected RunNotFound, got {result:?}"
    );
}

#[test]
fn domain_validation_runs_before_any_write() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");

    let blank = StartRunCommand {
        task: "  ".into(),
        ..start_command("ignored")
    };

    assert!(matches!(
        store.start_run(&blank, HarnessKind::ClaudeCode),
        Err(StoreError::Domain(_))
    ));
    assert_eq!(store.run_count().expect("run_count"), 0);
}

// --- concurrency -----------------------------------------------------------

/// The load-bearing test for dropping the daemon.
///
/// Several short-lived processes — one per hook invocation, plus the MCP server
/// — write the same file with no coordinator. If WAL and `busy_timeout` do not
/// hold, the design does not hold, so this uses real OS processes rather than
/// threads sharing one handle.
#[test]
fn two_processes_write_concurrently_without_losing_writes() {
    if let Ok(db) = std::env::var(CHILD_DB_ENV) {
        run_as_competing_writer(Path::new(&db));
        return;
    }

    let (_dir, path) = temp_db();
    Store::open(&path).expect("create schema");

    let children: Vec<_> = ["a", "b"]
        .iter()
        .map(|tag| {
            let child = Command::new(std::env::current_exe().expect("current_exe"))
                .args([
                    "two_processes_write_concurrently_without_losing_writes",
                    "--exact",
                    "--nocapture",
                ])
                .env(CHILD_DB_ENV, &path)
                .env(CHILD_TAG_ENV, tag)
                // Captured so a failure reports *why* the writer died. Without
                // this the parent only sees an exit code and the real error has
                // to be reproduced by hand.
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn competing writer");
            (*tag, child)
        })
        .collect();

    for (tag, child) in children {
        let output = child.wait_with_output().expect("wait");
        assert!(
            output.status.success(),
            "competing writer {tag} failed ({:?}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let store = Store::open(&path).expect("reopen");
    assert_eq!(
        store.total_checkpoint_count().expect("count"),
        2 * WRITES_PER_CHILD,
        "every concurrent write must survive"
    );
}

fn run_as_competing_writer(db: &Path) {
    let tag = std::env::var(CHILD_TAG_ENV).unwrap_or_else(|_| "child".into());
    let store = Store::open(db).expect("child open");

    let started = store
        .start_run(
            &start_command(&format!("concurrent writer {tag}")),
            HarnessKind::ClaudeCode,
        )
        .expect("child start");

    for index in 0..WRITES_PER_CHILD {
        store
            .save_checkpoint(&checkpoint_command(
                started.run_id,
                started.session_id,
                &format!("{tag}-{index}"),
            ))
            .expect("child checkpoint");
    }
}

// --- job queue -------------------------------------------------------------

#[test]
fn a_job_is_claimed_once_and_then_completed() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");

    store
        .enqueue_job("enrich_checkpoint", "run-1", "{\"transcript\":\"/tmp/x\"}")
        .expect("enqueue");

    let claimed = store
        .claim_job("enrich_checkpoint", Duration::from_mins(1))
        .expect("claim")
        .expect("a job is available");
    assert_eq!(claimed.job_key, "run-1");

    assert!(
        store
            .claim_job("enrich_checkpoint", Duration::from_mins(1))
            .expect("second claim")
            .is_none(),
        "a leased job must not be handed to a second worker"
    );

    store
        .complete_job("enrich_checkpoint", "run-1")
        .expect("complete");
    assert!(
        store
            .claim_job("enrich_checkpoint", Duration::from_mins(1))
            .expect("third claim")
            .is_none()
    );
}

#[test]
fn enqueueing_the_same_job_key_twice_does_not_duplicate_work() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");

    store
        .enqueue_job("enrich_checkpoint", "run-1", "{}")
        .expect("first");
    store
        .enqueue_job("enrich_checkpoint", "run-1", "{}")
        .expect("second");

    assert!(
        store
            .claim_job("enrich_checkpoint", Duration::from_mins(1))
            .expect("claim")
            .is_some()
    );
    assert!(
        store
            .claim_job("enrich_checkpoint", Duration::from_mins(1))
            .expect("claim")
            .is_none()
    );
}

// --- reentrancy ------------------------------------------------------------

/// The store guards one connection with a non-reentrant mutex, so a method that
/// calls another public method while still holding the guard deadlocks.
///
/// That failure mode is silent and total: it hangs the MCP server or a hook
/// rather than returning an error. Asserted with a timeout so a regression
/// fails the suite instead of stalling it forever.
#[test]
fn public_methods_never_deadlock_on_the_store_itself() {
    use std::sync::mpsc;

    let (_dir, path) = temp_db();
    let store = Store::open(&path).expect("open");
    let root = std::env::temp_dir();

    store
        .start_run(
            &start_command("something in flight"),
            HarnessKind::ClaudeCode,
        )
        .expect("start");

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = store.latest_open_run_for_path(&root);
        let _ = sender.send(outcome.map(|run| run.is_some()));
    });

    match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(found)) => assert!(found, "the open run should have been found"),
        Ok(Err(error)) => panic!("lookup failed: {error}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("latest_open_run_for_path deadlocked")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the lookup thread died"),
    }
}
