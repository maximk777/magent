//! The distillation worker.
//!
//! The worker turns a transcript into the reasoning a deterministic checkpoint
//! cannot capture. Every test here drives it through a fake `Distiller`: the
//! queue semantics — leasing, reclaiming, retrying, giving up — are what break
//! in production, and they must be provable without spending a model call.
//!
//! One test exercises the real engine. It is `#[ignore]`d because it needs an
//! authenticated `claude` and a network.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use magent_core::{CheckpointOrigin, HarnessKind, OperationId, StartRunCommand};
use magent_distill::{
    ClaudeHeadless, DistillRequest, Distillation, Distiller, Outcome, WorkerConfig, run_once,
};
use magent_store::Store;

const ENRICH: &str = "enrich_checkpoint";
const LEASE: Duration = Duration::from_mins(1);

/// Backoff is a parameter rather than a constant so tests can drive the retry
/// path without sleeping, and without a test-only hook on the store.
///
/// Takes the bound rather than the lease, because the lease is derived from it
/// now: a caller that could set the two apart is exactly what this change
/// removed.
fn config(distillation_timeout: Duration) -> WorkerConfig {
    WorkerConfig {
        distillation_timeout,
        retry_backoff: Duration::ZERO,
    }
}

/// Records how often it was asked to think, so tests can prove the worker did
/// not spend a model call it had no business spending.
struct FakeDistiller {
    calls: Arc<AtomicUsize>,
    outcome: Result<Distillation, &'static str>,
}

impl FakeDistiller {
    fn succeeding() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(Distillation {
                completed_steps: vec!["traced the owner".into()],
                next_steps: vec!["write the regression test".into()],
                decisions: vec!["keep the public API compatible".into()],
                rejected: vec!["rewriting the client".into()],
                verification: vec!["targeted test is red".into()],
                risks: vec![],
                handoff_summary: "owner traced; regression test is next".into(),
            }),
        }
    }

    fn failing() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Err("the model was unreachable"),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Distiller for FakeDistiller {
    fn distill(&self, _request: &DistillRequest) -> anyhow::Result<Distillation> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.outcome {
            Ok(distillation) => Ok(distillation.clone()),
            Err(message) => Err(anyhow::anyhow!(*message)),
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    transcript: PathBuf,
    run_id: magent_core::RunId,
    session_id: magent_core::SessionId,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("magent.db")).expect("open");

        let transcript = dir.path().join("transcript.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").expect("write transcript");

        let started = store
            .start_run(
                &StartRunCommand {
                    operation_id: OperationId::new(),
                    task: "fix the payment timeout".into(),
                    resume_run_id: None,
                    external_session_hint: None,
                    workspace_roots: vec![dir.path().to_path_buf()],
                },
                HarnessKind::ClaudeCode,
            )
            .expect("start run");

        Self {
            _dir: dir,
            store,
            transcript,
            run_id: started.run_id,
            session_id: started.session_id,
        }
    }

    fn enqueue(&self) {
        self.store
            .enqueue_job(
                ENRICH,
                &self.run_id.to_string(),
                &serde_json::json!({
                    "run_id": self.run_id,
                    "session_id": self.session_id,
                    "transcript_path": self.transcript,
                })
                .to_string(),
            )
            .expect("enqueue");
    }

    fn job_status(&self) -> Option<String> {
        self.store
            .job_state(ENRICH, &self.run_id.to_string())
            .expect("job_state")
            .map(|state| state.status)
    }
}

// --- the happy path --------------------------------------------------------

#[test]
fn a_distilled_job_becomes_an_enriched_checkpoint() {
    let fixture = Fixture::new();
    fixture.enqueue();
    let distiller = FakeDistiller::succeeding();

    let outcome = run_once(&fixture.store, &distiller, &config(LEASE)).expect("run_once");

    assert!(matches!(outcome, Outcome::Enriched(run) if run == fixture.run_id));
    assert_eq!(distiller.calls(), 1);
    assert_eq!(fixture.job_status().as_deref(), Some("done"));

    let checkpoint = fixture
        .store
        .get_run(fixture.run_id)
        .expect("get_run")
        .latest_checkpoint
        .expect("a checkpoint");

    assert_eq!(checkpoint.origin, CheckpointOrigin::Enriched);
    assert_eq!(
        checkpoint.handoff_summary,
        "owner traced; regression test is next"
    );
    assert_eq!(
        checkpoint.rejected,
        vec!["rewriting the client".to_string()]
    );
}

#[test]
fn an_empty_queue_costs_nothing() {
    let fixture = Fixture::new();
    let distiller = FakeDistiller::succeeding();

    let outcome = run_once(&fixture.store, &distiller, &config(LEASE)).expect("run_once");

    assert!(matches!(outcome, Outcome::Idle));
    assert_eq!(
        distiller.calls(),
        0,
        "an idle worker must never call the model"
    );
}

// --- queue semantics -------------------------------------------------------

/// The worker is spawned detached from hooks, so two can overlap. A job handled
/// twice would produce two checkpoints and pay for two model calls.
#[test]
fn a_leased_job_is_invisible_to_a_second_worker() {
    let fixture = Fixture::new();
    fixture.enqueue();

    let first = FakeDistiller::succeeding();
    run_once(&fixture.store, &first, &config(LEASE)).expect("first worker");

    let second = FakeDistiller::succeeding();
    let outcome = run_once(&fixture.store, &second, &config(LEASE)).expect("second worker");

    assert!(matches!(outcome, Outcome::Idle));
    assert_eq!(second.calls(), 0);
    assert_eq!(
        fixture
            .store
            .checkpoint_count(fixture.run_id)
            .expect("count"),
        1
    );
}

/// A worker killed mid-job must not park the work forever: the reasoning behind
/// a session is only worth distilling while it is still relevant.
#[test]
fn an_expired_lease_is_reclaimed() {
    let fixture = Fixture::new();
    fixture.enqueue();

    let abandoned = FakeDistiller::failing();
    // Claimed with a lease that is already over.
    let _ = run_once(
        &fixture.store,
        &abandoned,
        &config(Duration::from_millis(1)),
    );
    std::thread::sleep(Duration::from_millis(50));

    let later = FakeDistiller::succeeding();
    let outcome = run_once(&fixture.store, &later, &config(LEASE)).expect("later worker");

    assert!(matches!(outcome, Outcome::Enriched(_)));
    assert_eq!(later.calls(), 1);
}

// --- failure handling ------------------------------------------------------

#[test]
fn a_failure_schedules_a_retry_and_records_why() {
    let fixture = Fixture::new();
    fixture.enqueue();
    let distiller = FakeDistiller::failing();

    let outcome = run_once(&fixture.store, &distiller, &config(LEASE)).expect("run_once");

    assert!(matches!(outcome, Outcome::Failed(_)));

    let state = fixture
        .store
        .job_state(ENRICH, &fixture.run_id.to_string())
        .expect("job_state")
        .expect("the job still exists");

    assert_eq!(state.status, "pending", "a retryable failure stays queued");
    assert!(state.retry_remaining < 3, "a retry was consumed");
    assert!(
        state
            .last_error
            .unwrap_or_default()
            .contains("the model was unreachable"),
        "the reason must survive for diagnosis"
    );
}

/// A permanently broken job must stop costing money. Without a floor, a job
/// that can never succeed retries for as long as the database exists.
#[test]
fn a_job_gives_up_after_its_retries_are_exhausted() {
    let fixture = Fixture::new();
    fixture.enqueue();

    let mut attempts = 0;
    for _ in 0..10 {
        let distiller = FakeDistiller::failing();
        match run_once(&fixture.store, &distiller, &config(LEASE)).expect("run_once") {
            Outcome::Failed(_) => attempts += 1,
            Outcome::Idle => break,
            Outcome::Enriched(_) => panic!("a failing distiller must not succeed"),
        }
    }

    assert!(attempts >= 1);
    assert_eq!(
        fixture.job_status().as_deref(),
        Some("failed"),
        "an exhausted job stops being handed out"
    );
}

#[test]
fn a_vanished_transcript_fails_the_job_without_calling_the_model() {
    let fixture = Fixture::new();
    fixture.enqueue();
    std::fs::remove_file(&fixture.transcript).expect("remove transcript");

    let distiller = FakeDistiller::succeeding();
    let outcome = run_once(&fixture.store, &distiller, &config(LEASE)).expect("run_once");

    assert!(matches!(outcome, Outcome::Failed(_)));
    assert_eq!(
        distiller.calls(),
        0,
        "there is nothing to distil, so nothing should be paid for"
    );
}

// --- the real engine -------------------------------------------------------

/// Needs an authenticated `claude` and a network, so it is not part of the
/// suite. Run it by hand:
///
/// ```text
/// cargo test -p magent-distill -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires an authenticated claude CLI and network access"]
fn the_real_engine_produces_a_usable_distillation() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.transcript, transcript_fixture()).expect("write transcript");

    let engine = ClaudeHeadless::default();
    let distillation = engine
        .distill(&DistillRequest {
            run_id: fixture.run_id,
            session_id: fixture.session_id,
            task: "fix the payment timeout".into(),
            transcript: fixture.transcript.clone(),
        })
        .expect("distil");

    assert!(
        !distillation.handoff_summary.trim().is_empty(),
        "a distillation with no summary is useless"
    );
}

fn transcript_fixture() -> String {
    [
        r#"{"type":"user","message":{"content":"the payment call times out under load"}}"#,
        r#"{"type":"assistant","message":{"content":"The retry budget is hardcoded to 3 in src/client.rs. I will make it configurable rather than raising it, because raising it hides the latency problem."}}"#,
        r#"{"type":"assistant","message":{"content":"Added a failing test in tests/retry.rs; it is red as expected."}}"#,
    ]
    .join("\n")
}

/// Guards the invariant that the engine cannot recurse.
///
/// The distiller runs `claude`, the CLI this integration hooks into. Left
/// alone that nested session would open a run and, on compaction, queue
/// another distillation — a loop that bills itself. The guard is an
/// environment variable the hook honours, not a flag on the CLI: `--bare`
/// used to serve here and also skipped keychain reads, so on a subscription
/// the engine could not authenticate at all.
#[test]
fn the_engine_disables_the_integration_it_runs_under() {
    let arguments = ClaudeHeadless::default().command_arguments();

    assert!(
        !arguments.iter().any(|argument| argument == "--bare"),
        "--bare forces API-key auth, so a subscription cannot distil: {arguments:?}"
    );
    assert!(arguments.iter().any(|argument| argument == "-p"));
    assert_eq!(magent_distill::RECURSION_GUARD, "MAGENT_DISTILLING");
}

/// The lease exists so a dead worker's job is picked up. A lease shorter than
/// the bound would do the opposite: hand the job to a second worker while the
/// first is still running and still going to write its result.
#[test]
fn the_lease_outlasts_the_bound_it_exists_to_survive() {
    let shipped = WorkerConfig::default();
    assert!(
        shipped.lease() > shipped.distillation_timeout,
        "a lease shorter than the bound hands the same job to a second worker \
         while the first is still running: {:?} vs {:?}",
        shipped.lease(),
        shipped.distillation_timeout
    );

    let tighter = WorkerConfig {
        distillation_timeout: Duration::from_secs(7),
        ..WorkerConfig::default()
    };
    assert!(
        tighter.lease() > tighter.distillation_timeout,
        "the relationship must hold for any bound, not only the shipped one"
    );
}

// --- the bound on a child --------------------------------------------------

/// A stand-in for `claude`, so these tests never call a model.
fn fake_claude(dir: &std::path::Path, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write the fake");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// The bound under test. Short enough that a test waits a second rather than
/// three minutes, and the margin below is what tells a bound that worked from
/// one that never fired.
const TEST_BOUND: Duration = Duration::from_secs(1);
const MARGIN: Duration = Duration::from_secs(10);

fn request_for(fixture: &Fixture) -> DistillRequest {
    DistillRequest {
        run_id: fixture.run_id,
        session_id: fixture.session_id,
        task: "fix the payment timeout".into(),
        transcript: fixture.transcript.clone(),
    }
}

#[test]
fn a_child_that_never_exits_is_stopped_at_the_bound() {
    let fixture = Fixture::new();
    let binary = fake_claude(
        fixture.transcript.parent().expect("dir"),
        "sleeper",
        "sleep 600",
    );
    let engine = ClaudeHeadless::new(binary, "haiku".into(), TEST_BOUND);

    let started = std::time::Instant::now();
    let error = engine
        .distill(&request_for(&fixture))
        .expect_err("a child past its bound must fail");

    assert!(
        started.elapsed() < MARGIN,
        "the bound never fired: took {:?}",
        started.elapsed()
    );
    assert!(
        error.to_string().to_lowercase().contains("within"),
        "the failure must name the bound: {error}"
    );
}

/// The test that tells a real bound from a decorative one.
///
/// A pipe holds about sixty-four kilobytes. This child writes far more and then
/// hangs, so it blocks in `write` unless the worker drains it — and a worker
/// polling for exit while nobody reads would wait for ever on exactly the child
/// the bound exists to catch.
#[test]
fn a_child_that_floods_its_pipe_and_hangs_is_still_stopped() {
    let fixture = Fixture::new();
    let binary = fake_claude(
        fixture.transcript.parent().expect("dir"),
        "flooder",
        "head -c 1000000 /dev/zero | tr '\\0' 'x'; sleep 600",
    );
    let engine = ClaudeHeadless::new(binary, "haiku".into(), TEST_BOUND);

    let started = std::time::Instant::now();
    let error = engine
        .distill(&request_for(&fixture))
        .expect_err("a flooding child past its bound must fail");

    assert!(
        started.elapsed() < MARGIN,
        "the worker blocked on the undrained pipe: took {:?}",
        started.elapsed()
    );
    assert!(
        error.to_string().to_lowercase().contains("within"),
        "the failure must name the bound: {error}"
    );
}

#[test]
fn a_child_that_answers_at_once_is_never_stopped() {
    let fixture = Fixture::new();
    let reply = r#"{\"handoff_summary\":\"did the thing\",\"completed_steps\":[],\"next_steps\":[],\"decisions\":[],\"rejected\":[],\"verification\":[],\"risks\":[]}"#;
    let binary = fake_claude(
        fixture.transcript.parent().expect("dir"),
        "prompt-answer",
        &format!(r#"printf '%s' '{{"is_error":false,"result":"{reply}"}}'"#),
    );
    let engine = ClaudeHeadless::new(binary, "haiku".into(), TEST_BOUND);

    let distillation = engine
        .distill(&request_for(&fixture))
        .expect("a child inside its bound is left alone");

    assert_eq!(distillation.handoff_summary, "did the thing");
}
