//! Closing one task of a plan, with the evidence that proved it.
//!
//! The tick rides on a checkpoint rather than on a verb of its own, because a
//! checkpoint is where the agent has just finished something and knows what
//! proved it. It lands in the checkpoint's own transaction, so `status`,
//! `evidence` and `verified_at` move together — which is what
//! `0009_tasks.sql` means by "evidence and `verified_at` land together": a
//! checked box with no evidence is a claim the next session cannot audit and
//! will build on regardless.
//!
//! What the command printed is recorded exactly as it came, and the plan's
//! `expected_output` markers are only *reported* on: the tick names the ones
//! the output does not carry. They are written before the work is done, so
//! refusing a tick over them would stop correct work.

use magent_core::{
    ArchiveCommand, ChangeId, ChangeStatus, CheckpointCommand, CheckpointOrigin, CheckpointResult,
    Classification, DeltaOp, FileLedgerEntry, HarnessKind, OperationId, PlanCommand,
    ProposeCommand, RequirementDraft, RunId, ScenarioDraft, SessionId, SpecBinding, SpecifyCommand,
    StartRunCommand, TaskDone, TaskDraft, WorkflowStage,
};
use magent_store::{FactContext, Store, StoreError};
use rusqlite::Connection;

const SLUG: &str = "add-retry-budget";
const CAPABILITY: &str = "worker/retry";
const REQUIREMENT: &str = "The worker stops retrying once its budget is spent";
const VERIFY: &str = "cargo test -p worker retry";
const EXPECTED: &str = "test result: ok. 3 passed";

/// Long enough to clear `magent-core`'s 50-character floor on a purpose.
const PURPOSE: &str = "Retrying work that failed for a reason that may not repeat, without \
                       hammering a service that is already struggling.";

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Store,
    context: FactContext,
    root: std::path::PathBuf,
}

impl Fixture {
    /// A store in a throwaway directory: nothing here can reach a real
    /// profile, because the path is handed in rather than discovered.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("magent.db");
        let store = Store::open(&path).expect("open");

        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir");
        let resolved = store.resolve_workspace_for(&root).expect("resolve");
        let context = FactContext {
            workspace_id: Some(resolved.workspace_id),
            namespace: None,
            ..FactContext::default()
        };

        Self {
            _dir: dir,
            path,
            store,
            context,
            root,
        }
    }

    /// A change carried all the way to `planned`, with two tasks on it — so a
    /// tick closing one leaves the plan unfinished.
    ///
    /// The task under test is numbered "1.3" rather than "1": plans are
    /// numbered hierarchically, which is why `tasks.number` is `TEXT`
    /// (`0009_tasks.sql`), and a fixture whose numbers are all single digits
    /// would pass whether or not the number is carried as written.
    fn planned_change(&self) -> ChangeId {
        self.planned_change_expecting(&[EXPECTED])
    }

    /// The same change, with the markers task "1.3" is planned to expect
    /// handed in — so a test can plan more than one of them and read back
    /// which ones the tick did not find.
    fn planned_change_expecting(&self, markers: &[&str]) -> ChangeId {
        let change = self.proposed_and_specified();

        self.store
            .plan(
                &PlanCommand {
                    operation_id: OperationId::new(),
                    change,
                    tasks: vec![
                        task("1.3", &[REQUIREMENT], markers),
                        task("2", &[], &[EXPECTED]),
                    ],
                    check_only: false,
                },
                &self.context,
            )
            .expect("plan");

        change
    }

    /// A change carried to `specified` — the propose-then-specify half every
    /// planned change in this file shares, before its tasks are written.
    fn proposed_and_specified(&self) -> ChangeId {
        let change = self
            .store
            .propose(
                &ProposeCommand {
                    operation_id: OperationId::new(),
                    slug: SLUG.into(),
                    title: "Add a retry budget".into(),
                    classification: Classification::Bounded,
                    why: "Retries currently have no ceiling and can loop forever.".into(),
                    what_changes: vec!["Add a configurable retry budget".into()],
                    capabilities: vec![CAPABILITY.into()],
                    impact: Some("None known.".into()),
                    skip_specs: false,
                },
                &self.context,
            )
            .expect("propose")
            .id;

        self.store
            .specify(
                &SpecifyCommand {
                    operation_id: OperationId::new(),
                    change,
                    capability_path: CAPABILITY.into(),
                    purpose: Some(PURPOSE.into()),
                    requirements: vec![RequirementDraft {
                        op: DeltaOp::Added,
                        name: REQUIREMENT.into(),
                        text: Some(
                            "The worker SHALL stop retrying once the budget is spent.".into(),
                        ),
                        rename_to: None,
                        reason: None,
                        migration: None,
                        scenarios: vec![ScenarioDraft {
                            name: "budget exhausted".into(),
                            given: None,
                            when: "the budget is exhausted".into(),
                            then: "the job is parked".into(),
                        }],
                    }],
                },
                &self.context,
            )
            .expect("specify");

        change
    }

    /// A change planned with tasks "3" and "4" instead of "1.3" and "2":
    /// task "3" declares nothing that overlaps task "4", and task "4"
    /// declares `src/store.rs`. Built for the trespass-refusal tests, whose
    /// plan text names task 3 as the one that closes and task 4 as the one
    /// that holds the file the close finds trespassed.
    fn planned_change_for_trespass(&self) -> ChangeId {
        let change = self.proposed_and_specified();

        self.store
            .plan(
                &PlanCommand {
                    operation_id: OperationId::new(),
                    change,
                    tasks: vec![
                        task("3", &[REQUIREMENT], &[EXPECTED]),
                        task_declaring("4", &["src/store.rs"], &[EXPECTED]),
                    ],
                    check_only: false,
                },
                &self.context,
            )
            .expect("plan");

        change
    }

    /// A run of this workspace that names no change: the shape a run has
    /// before anything binds it, and the one a tick has nothing to resolve
    /// against.
    fn unbound_run(&self) -> (RunId, SessionId) {
        let started = self
            .store
            .start_run(
                &StartRunCommand {
                    operation_id: OperationId::new(),
                    task: "Cap the retry loop".into(),
                    resume_run_id: None,
                    external_session_hint: None,
                    workspace_roots: vec![self.root.clone()],
                },
                HarnessKind::ClaudeCode,
            )
            .expect("start");

        (started.run_id, started.session_id)
    }

    /// A run of this workspace, bound to the change by its slug — which is
    /// what `runs.spec_change_id` holds.
    fn bound_run(&self) -> (RunId, SessionId) {
        let (run_id, session_id) = self.unbound_run();

        self.store
            .bind_spec(
                run_id,
                &SpecBinding {
                    change_id: Some(SLUG.into()),
                    current_task: Some("1.3: cap the loop".into()),
                },
            )
            .expect("bind");

        (run_id, session_id)
    }

    /// A run bound to the change by its slug, but naming no task — nothing
    /// is held until a test calls [`Fixture::hold`]. For the trespass tests,
    /// which need to choose which task each session holds rather than
    /// inheriting `bound_run`'s fixed "1.3".
    fn bound_run_unclaimed(&self) -> (RunId, SessionId) {
        let (run_id, session_id) = self.unbound_run();

        self.store
            .bind_spec(
                run_id,
                &SpecBinding {
                    change_id: Some(SLUG.into()),
                    current_task: None,
                },
            )
            .expect("bind");

        (run_id, session_id)
    }

    /// Claims a task by naming it on a binding — the way production takes a
    /// hold; there is no claim verb of its own.
    fn hold(&self, run_id: RunId, current_task: &str) {
        self.store
            .bind_spec(
                run_id,
                &SpecBinding {
                    change_id: None,
                    current_task: Some(current_task.into()),
                },
            )
            .expect("hold");
    }

    /// A second session joined to `run_id`, by binding to the same
    /// workspace root, which `bind_session` resolves back to the run already
    /// open on it. `write_binding`'s claim (`store.rs`) hands a hold to
    /// whichever session on the run was seen most recently, so calling this
    /// right before `hold` is what makes the claim land on the session it
    /// just created rather than on whichever session held the previous task.
    fn second_session(&self, run_id: RunId, hint: &str) -> SessionId {
        let bound = self
            .store
            .bind_session(
                hint,
                &self.root,
                "cap the retry loop",
                HarnessKind::ClaudeCode,
            )
            .expect("bind second session");
        assert_eq!(bound.run_id, run_id, "both sessions must share one run");
        bound.session_id
    }

    /// The session currently claiming a task, read back to confirm
    /// `second_session` actually won the hold it was meant to win, before a
    /// test trusts anything built on that assumption.
    fn holder_of(&self, change: ChangeId, task_number: &str) -> Option<SessionId> {
        let claimed_by: Option<String> = self
            .raw()
            .query_row(
                "SELECT claimed_by FROM tasks WHERE number = ?1 AND change_id = ?2",
                rusqlite::params![task_number, change.to_string()],
                |row| row.get(0),
            )
            .expect("one task row");
        claimed_by.and_then(|id| id.parse().ok())
    }

    /// A checkpoint carrying a tick, under an `operation_id` the caller picks.
    ///
    /// The id is an argument and the refusal comes back rather than being
    /// unwrapped here: a replay is the same command sent twice, so a test has
    /// to be able to repeat one id, and a tick that cannot be placed answers
    /// with a [`StoreError`] a test has to be able to read.
    fn checkpoint(
        &self,
        run_id: RunId,
        session_id: SessionId,
        operation_id: OperationId,
        task_done: TaskDone,
    ) -> Result<CheckpointResult, StoreError> {
        self.checkpoint_with_binding(run_id, session_id, operation_id, task_done, None)
    }

    /// The same, with the binding the tick is to be resolved against carried on
    /// the checkpoint itself — the shape a first checkpoint of a task has, where
    /// nothing has bound the run yet.
    fn checkpoint_with_binding(
        &self,
        run_id: RunId,
        session_id: SessionId,
        operation_id: OperationId,
        task_done: TaskDone,
        binding: Option<SpecBinding>,
    ) -> Result<CheckpointResult, StoreError> {
        self.store.save_checkpoint(&CheckpointCommand {
            operation_id,
            run_id,
            session_id,
            stage: WorkflowStage::Executing,
            origin: CheckpointOrigin::Enriched,
            completed_steps: vec!["capped the retry loop".into()],
            next_steps: vec![],
            decisions: vec![],
            rejected: vec![],
            changed_files: vec![],
            verification: vec![],
            risks: vec![],
            handoff_summary: "The budget is read from config and spent per attempt.".into(),
            task_done: Some(task_done),
            binding,
        })
    }

    /// The task row as it now stands: status, evidence, `verified_at`.
    ///
    /// Scoped by change as well as by number, because `tasks_number`
    /// (`0009_tasks.sql`) is unique on the pair and not on the number: one
    /// workspace can hold two changes whose plans both start at "1", and a
    /// query on the number alone would answer from whichever row came first.
    fn task_row(&self, change: ChangeId, number: &str) -> (String, Option<String>, Option<String>) {
        self.raw()
            .query_row(
                "SELECT status, evidence, verified_at FROM tasks
                 WHERE change_id = ?1 AND number = ?2",
                rusqlite::params![change.to_string(), number],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("task row")
    }

    /// Where the change now stands, read back the way a caller sees it.
    ///
    /// Through the store rather than off the column, so that a status the store
    /// writes but cannot read back — `enum_from_sql` refuses one it does not
    /// know — fails here rather than passing as a string comparison.
    fn change_status(&self, change: ChangeId) -> ChangeStatus {
        self.store
            .change_detail(change, &self.context, None)
            .expect("change detail")
            .expect("the change is this workspace's")
            .status
    }

    /// A second connection to the same file, for the assertions and the
    /// triggers the store itself would never write.
    fn raw(&self) -> Connection {
        Connection::open(&self.path).expect("raw connection")
    }
}

fn row_count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

/// Every tick in the journal, oldest first.
///
/// Ordered by `rowid` rather than by `created_at`: two ticks of one session
/// share a timestamp readily, and the question here is the order they were
/// written in.
fn ticks(connection: &Connection) -> Vec<(String, String)> {
    connection
        .prepare("SELECT number, output FROM task_ticks ORDER BY rowid")
        .expect("task_ticks")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("ticks")
}

/// The task numbers the plan now holds.
fn task_numbers(connection: &Connection) -> Vec<String> {
    connection
        .prepare("SELECT number FROM tasks ORDER BY number")
        .expect("tasks")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("numbers")
}

fn task(number: &str, covers: &[&str], markers: &[&str]) -> TaskDraft {
    TaskDraft {
        number: number.into(),
        title: format!("Cap the retry loop, step {number}"),
        body: Some("Read the budget from config and stop once it is spent.".into()),
        files: vec!["crates/worker/src/retry.rs".into()],
        consumes: Vec::new(),
        produces: vec!["fn spend_budget(&mut self) -> bool".into()],
        verify_command: VERIFY.into(),
        expected_output: markers.iter().map(|marker| (*marker).to_string()).collect(),
        covers: covers.iter().map(|name| (*name).to_string()).collect(),
    }
}

/// Like `task`, but with the files it declares under the caller's control —
/// for the trespass tests, which need a task declaring `src/store.rs` rather
/// than `task`'s fixed `crates/worker/src/retry.rs`.
fn task_declaring(number: &str, files: &[&str], markers: &[&str]) -> TaskDraft {
    TaskDraft {
        files: files.iter().map(|path| (*path).to_string()).collect(),
        ..task(number, &[], markers)
    }
}

#[test]
fn task_closes_with_its_evidence() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let output = format!("running 3 tests\n...\n{EXPECTED}; finished in 0.42s\n");
    let result = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: output.clone(),
            },
        )
        .expect("checkpoint");

    let closed = result.task.expect("the checkpoint closed a task");
    assert_eq!(closed.number, "1.3");
    assert!(
        closed.expected_output_missing.is_empty(),
        "the plan's expected_output is in this output"
    );
    assert!(
        !closed.change_ready,
        "task 2 of the plan is still open, so the change is not ready"
    );

    let (status, evidence, verified_at) = fixture.task_row(change, "1.3");
    assert_eq!(status, "done");
    assert_eq!(
        evidence.as_deref(),
        Some(output.as_str()),
        "the evidence is what the command printed, as it printed it"
    );
    assert!(
        verified_at.is_some(),
        "evidence and verified_at land together"
    );
}

/// The comparison is reported, never enforced. `expected_output` is written
/// while the plan is being drafted — before anyone has run the command — so a
/// tick that does not match it is still the evidence of what happened, and
/// throwing it away would leave the task open with the work already done.
#[test]
fn evidence_is_recorded_even_when_the_output_differs() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let output = "test result: ok. 4 passed; 1 ignored\n";
    let result = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: output.into(),
            },
        )
        .expect("checkpoint");

    let closed = result.task.expect("the checkpoint closed a task");
    assert_eq!(
        closed.expected_output_missing,
        [EXPECTED],
        "{EXPECTED:?} does not appear in {output:?}"
    );

    let (status, evidence, verified_at) = fixture.task_row(change, "1.3");
    assert_eq!(status, "done", "the task closes either way");
    assert_eq!(evidence.as_deref(), Some(output));
    assert!(verified_at.is_some());
}

/// Which markers were missed, rather than that something was.
///
/// A boolean says only that the tick and its plan disagree somewhere, and a
/// reader of that cannot tell a renamed test from a run that genuinely failed.
/// Naming the marker the output does not carry makes the difference readable:
/// here the suite passed and the count moved, and only the count is reported.
#[test]
fn a_tick_names_the_markers_it_did_not_find() {
    let fixture = Fixture::new();
    let change = fixture.planned_change_expecting(&["test result: ok", "7 passed"]);
    let (run_id, session_id) = fixture.bound_run();

    let output = format!("running 3 tests\n...\n{EXPECTED}; finished in 0.42s\n");
    let result = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: output.clone(),
            },
        )
        .expect("checkpoint");

    let closed = result.task.expect("the checkpoint closed a task");
    assert_eq!(
        closed.expected_output_missing,
        ["7 passed"],
        "the suite passed, so only the count is missing from {output:?}"
    );

    let (status, evidence, verified_at) = fixture.task_row(change, "1.3");
    assert_eq!(status, "done", "the task closes either way");
    assert_eq!(evidence.as_deref(), Some(output.as_str()));
    assert!(verified_at.is_some());
}

/// The empty list is the good news, and it is the same list: a caller reads
/// one field either way rather than a flag it has to pair with a reason.
#[test]
fn a_tick_that_matches_every_marker_reports_none() {
    let fixture = Fixture::new();
    let change = fixture.planned_change_expecting(&["test result: ok", "7 passed"]);
    let (run_id, session_id) = fixture.bound_run();

    let output = "running 7 tests\n...\ntest result: ok. 7 passed; finished in 0.42s\n";
    let result = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: output.into(),
            },
        )
        .expect("checkpoint");

    let closed = result.task.expect("the checkpoint closed a task");
    assert!(
        closed.expected_output_missing.is_empty(),
        "every marker the plan named is in {output:?}, and {:?} says otherwise",
        closed.expected_output_missing
    );

    let (status, _, verified_at) = fixture.task_row(change, "1.3");
    assert_eq!(status, "done");
    assert!(verified_at.is_some());
}

/// The tick is part of the checkpoint's own operation, not a write that
/// follows it. The task is made unclosable from outside the store, so nothing
/// about the command is wrong and the failure lands mid-transaction: what must
/// not survive is a checkpoint that reads as if the work had been signed off.
#[test]
fn a_tick_that_cannot_be_written_takes_the_checkpoint_with_it() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let raw = fixture.raw();
    raw.execute_batch(
        "CREATE TRIGGER refuse_the_tick BEFORE UPDATE ON tasks
         BEGIN SELECT RAISE(ABORT, 'the task cannot be closed'); END;",
    )
    .expect("trigger");

    let error = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected the blocked tick to be a refusal");

    assert!(
        matches!(&error, StoreError::Database(message)
            if message.contains("the task cannot be closed")),
        "expected the blocked update to be reported, got {error:?}"
    );

    assert_eq!(
        row_count(&raw, "checkpoints"),
        0,
        "a checkpoint whose tick could not be written must not survive it"
    );
    let (status, evidence, verified_at) = fixture.task_row(change, "1.3");
    // `running`, not `pending`: binding the run named this task, which is what
    // claims it. "As it was" means as the refused tick found it.
    assert_eq!(status, "running", "and the task is left as it was");
    assert_eq!(evidence, None);
    assert_eq!(verified_at, None);
}

/// A retry after a crash reaches the store as the same command under the same
/// `operation_id`, and is answered from the recorded response rather than run
/// again. So what a tick did has to survive a round trip through that record:
/// answering the second call with no tick would tell a caller its evidence was
/// never filed, and send it looking for a task it had already closed.
#[test]
fn a_repeated_checkpoint_answers_with_the_same_tick() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let operation_id = OperationId::new();
    let done = TaskDone {
        number: "1.3".into(),
        verify_command: VERIFY.into(),
        output: format!("{EXPECTED}\n"),
    };

    let first = fixture
        .checkpoint(run_id, session_id, operation_id, done.clone())
        .expect("first checkpoint");
    let second = fixture
        .checkpoint(run_id, session_id, operation_id, done)
        .expect("replayed checkpoint");

    assert_eq!(first, second, "the replay answers what the first call did");
    assert!(
        second.task.is_some(),
        "and that answer still carries the tick"
    );

    let raw = fixture.raw();
    assert_eq!(
        row_count(&raw, "checkpoints"),
        1,
        "the replay must not have recorded a second checkpoint"
    );
    let (status, _, _) = fixture.task_row(change, "1.3");
    assert_eq!(status, "done");
}

/// The cheapest and most fundamental refusal, so it comes first: the slug a
/// task number is looked up in comes off the run, and a run bound to nothing
/// leaves no plan for any of the later checks to be about.
#[test]
fn a_tick_on_an_unbound_run_is_refused() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.unbound_run();

    let error = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected a tick on an unbound run to be refused");

    assert!(
        matches!(&error, StoreError::RunNotBoundToChange { run } if *run == run_id),
        "expected the unbound run to be named, got {error:?}"
    );
    assert_eq!(error.code(), "run_not_bound");

    let (status, _, _) = fixture.task_row(change, "1.3");
    assert_eq!(status, "pending", "and nothing was closed");
}

/// The first checkpoint of a task is one message: it names the change and the
/// task in hand, and ticks off what it just proved. Nothing has bound the run
/// before it, because until this task started there was nothing to bind it for.
///
/// So the binding has to land in this transaction and land before the tick,
/// which resolves its slug off the run. A binding written after the tick, or in
/// a call following the checkpoint, refuses this with `run_not_bound`.
#[test]
fn one_message_binds_the_run_and_closes_its_first_task() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.unbound_run();

    let result = fixture
        .checkpoint_with_binding(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
            Some(SpecBinding {
                change_id: Some(SLUG.into()),
                current_task: Some("1.3: cap the loop".into()),
            }),
        )
        .expect("a checkpoint carrying its own binding closes its first task");

    let closed = result.task.expect("the checkpoint closed a task");
    assert_eq!(closed.number, "1.3");

    let (status, evidence, verified_at) = fixture.task_row(change, "1.3");
    assert_eq!(status, "done");
    assert!(evidence.is_some() && verified_at.is_some());

    let spec = fixture
        .store
        .snapshot(run_id)
        .expect("snapshot")
        .spec
        .expect("the binding this checkpoint carried");
    assert_eq!(spec.change_id.as_deref(), Some(SLUG));
    assert_eq!(
        spec.current_task.as_deref(),
        Some("1.3: cap the loop"),
        "the run is left pointing at the task the tick was for"
    );
}

/// The numbers still open travel with the refusal, the way
/// `ChangeNotExecuted` and `CapabilityNotProposed` carry their lists: a caller
/// told only that its number is wrong has to go and read the plan out of the
/// database to find out which one is right.
#[test]
fn a_tick_for_an_unknown_number_lists_the_open_ones() {
    let fixture = Fixture::new();
    fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let error = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected a number no task has to be refused");

    let StoreError::TaskNotFound { slug, number, open } = &error else {
        panic!("expected the number to be reported as unknown, got {error:?}");
    };
    assert_eq!(number, "3");
    assert_eq!(
        slug, SLUG,
        "the plan is named, so a caller holding a stale binding can see it is the wrong plan"
    );
    assert_eq!(
        open,
        &["1.3".to_owned(), "2".to_owned()],
        "both tasks of this plan are still open, in `open_task_numbers`' lexicographic \
         order on the number, which for these two coincides with the plan's"
    );
    assert_eq!(error.code(), "task_not_found");

    let message = error.to_string();
    assert!(
        message.contains("1.3") && message.contains('2'),
        "the open numbers belong in the message, not only in the variant: {message}"
    );
    assert!(
        message.contains(SLUG),
        "and so does the plan they belong to: {message}"
    );
}

/// The one check that makes "run the command the plan named" enforceable
/// rather than advisory. The command sent here is a prefix of the planned one —
/// what a fuzzy comparison would wave through, and waving it through would put
/// back the hole this closes, since the evidence would then be of some other
/// command's output.
#[test]
fn a_tick_with_another_command_is_refused_and_leaves_the_task_open() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let error = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: "cargo test -p worker".into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected another command to be refused");

    assert!(
        matches!(&error, StoreError::VerifyCommandMismatch { number, expected }
            if number == "1.3" && expected == VERIFY),
        "expected the planned command to be carried, got {error:?}"
    );
    assert_eq!(error.code(), "verify_command_mismatch");
    assert!(
        error.to_string().contains(VERIFY),
        "the refusal names the command to run instead: {error}"
    );

    let (status, evidence, verified_at) = fixture.task_row(change, "1.3");
    // Still held rather than pending: the run named it, and a refused tick
    // does not take the task away from the agent that has it.
    assert_eq!(status, "running", "the task is left open for the real run");
    assert_eq!(evidence, None);
    assert_eq!(verified_at, None);
}

/// The likeliest of these refusals to fire for real: a run outlives the change
/// it was bound to, because archiving drops the slug out of the live set. The
/// binding is what has to be corrected, so the refusal names the slug rather
/// than the number — a caller told only "no task found" would go looking through
/// a plan that is not the one it is bound to.
#[test]
fn a_tick_whose_slug_names_no_open_change_is_refused() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    // `bind_spec` COALESCEs, so a non-null slug overwrites the one that is
    // there: this is the shape a run has after the change under it went away.
    fixture
        .store
        .bind_spec(
            run_id,
            &SpecBinding {
                change_id: Some("a-change-that-is-gone".into()),
                current_task: None,
            },
        )
        .expect("rebind");

    let error = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected a slug no open change answers to be refused");

    assert!(
        matches!(&error, StoreError::ChangeSlugNotFound(slug)
            if slug == "a-change-that-is-gone"),
        "expected the slug to be named, got {error:?}"
    );
    assert_eq!(error.code(), "change_slug_not_found");
    assert!(
        error.to_string().contains("re-bind"),
        "the refusal says what to do about it: {error}"
    );

    let (status, _, _) = fixture.task_row(change, "1.3");
    assert_eq!(
        status, "running",
        "the plan that does exist is left untouched"
    );
}

/// The one branch of the five with real work in it — the `len() > 1` guard and
/// the namespace mapping — and the one where getting it wrong is invisible:
/// resolving to whichever change sorted first would close a task on a plan
/// nobody did this work for, and report success.
#[test]
fn a_tick_whose_slug_names_two_changes_is_refused() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    // The live-slug index is per namespace, so the same slug can be proposed
    // again under another one. Neither specify nor plan is needed: this refusal
    // fires before any task is looked up.
    let elsewhere = FactContext {
        namespace: Some("infra".into()),
        ..fixture.context.clone()
    };
    fixture
        .store
        .propose(
            &ProposeCommand {
                operation_id: OperationId::new(),
                slug: SLUG.into(),
                title: "Add a retry budget, over there".into(),
                classification: Classification::Bounded,
                why: "The same slug, proposed under another namespace.".into(),
                what_changes: vec!["Add a configurable retry budget".into()],
                capabilities: vec![CAPABILITY.into()],
                impact: None,
                skip_specs: false,
            },
            &elsewhere,
        )
        .expect("propose elsewhere");

    let error = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected a slug two open changes answer to be refused");

    let StoreError::ChangeSlugAmbiguous { slug, namespaces } = &error else {
        panic!("expected the slug to be reported as ambiguous, got {error:?}");
    };
    assert_eq!(slug, SLUG);
    assert_eq!(
        namespaces,
        &["(no namespace)".to_owned(), "infra".to_owned()],
        "both namespaces, the one filed under none named rather than blank, ordered by \
         `change_by_slug`'s `IFNULL(namespace, '')` — a reordering here is that query's, \
         not this refusal's"
    );
    assert_eq!(error.code(), "change_slug_ambiguous");
    assert!(
        error.to_string().contains("infra"),
        "the namespaces that tell the two apart belong in the message: {error}"
    );

    let (status, _, _) = fixture.task_row(change, "1.3");
    assert_eq!(
        status, "running",
        "neither plan is closed while it is unclear which one this is"
    );
}

/// `ready` is the status `0009_tasks.sql` promises a change reaches when its
/// tasks are all done, and the tick that closes the last one is the only thing
/// that could write it. Both ticks are asserted, not just the second: a
/// readiness worked out from the wrong set of tasks would move the change on the
/// first tick, and a test that only looked after the last one could not tell
/// that apart from the right answer.
#[test]
fn closing_the_last_task_makes_the_change_ready() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let first = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect("first checkpoint")
        .task
        .expect("the checkpoint closed a task");

    assert!(
        !first.change_ready,
        "task 2 is still open, so this tick did not finish the plan"
    );
    assert_eq!(
        fixture.change_status(change),
        ChangeStatus::Planned,
        "and the change stays where planning left it"
    );

    let last = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "2".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect("last checkpoint")
        .task
        .expect("the checkpoint closed a task");

    assert!(
        last.change_ready,
        "nothing is open now, so this tick finished the plan"
    );
    assert_eq!(
        fixture.change_status(change),
        ChangeStatus::Ready,
        "which is what moves the change to `ready`"
    );
}

/// The end of the loop, from the tick that closes the last task to the deltas
/// landing in the live base.
///
/// It pins the requirement's scenario rather than this commit's write: both
/// halves read `tasks.status`, so archiving became reachable with the tick
/// itself and not with the `ready` status, and no mutation of the readiness
/// write can fail this test. What it does hold is that `ready` is a status
/// archiving accepts — a future `require_archivable_change` whitelisting only
/// `planned` would leave a change reported ready that archiving refuses, which
/// is a state a caller has no way out of.
#[test]
fn an_archive_after_the_last_tick_folds_the_deltas() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    for number in ["1.3", "2"] {
        fixture
            .checkpoint(
                run_id,
                session_id,
                OperationId::new(),
                TaskDone {
                    number: number.into(),
                    verify_command: VERIFY.into(),
                    output: format!("{EXPECTED}\n"),
                },
            )
            .expect("checkpoint");
    }

    let report = fixture
        .store
        .archive(
            &ArchiveCommand {
                operation_id: OperationId::new(),
                change,
            },
            &fixture.context,
        )
        .expect("a change whose every task is ticked can be archived");

    assert_eq!(report.added, 1, "the one requirement this change proposed");
    assert_eq!((report.modified, report.removed, report.renamed), (0, 0, 0));
    assert_eq!(
        report.capabilities_created,
        vec![CAPABILITY.to_owned()],
        "the capability the delta was filed under existed nowhere before this"
    );
    assert_eq!(report.status, ChangeStatus::Archived);
    assert_eq!(fixture.change_status(change), ChangeStatus::Archived);
}

/// `Store::plan` deletes the change's tasks so that a replan replaces the plan
/// rather than appending to it, and so a reused number is not fighting
/// `tasks_number`. Replanning a change already under way is legal, so the rows
/// that go can be ones a tick had written its evidence onto — and the evidence
/// would go with them if `tasks` were the only place it lived.
///
/// A plan is a statement about what remains to be done; a tick is a record of
/// something that happened, and nothing that happened stops having happened
/// because the plan changed.
#[test]
fn a_replan_leaves_the_journal_alone() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let output = format!("running 3 tests\n...\n{EXPECTED}; finished in 0.42s\n");
    fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: VERIFY.into(),
                output: output.clone(),
            },
        )
        .expect("checkpoint");

    fixture
        .store
        .plan(
            &PlanCommand {
                operation_id: OperationId::new(),
                change,
                tasks: vec![task("9", &[REQUIREMENT], &[EXPECTED])],
                check_only: false,
            },
            &fixture.context,
        )
        .expect("replanning a change under way is legal");

    let connection = fixture.raw();
    assert_eq!(
        task_numbers(&connection),
        vec!["9".to_owned()],
        "the replan replaced the plan, the row task 1.3 was closed on included"
    );
    assert_eq!(
        ticks(&connection),
        vec![("1.3".to_owned(), output)],
        "and the tick against 1.3 outlived the row it closed"
    );
}

/// The journal records what happened, so a tick that did not happen leaves
/// nothing behind. The insert sits inside the checkpoint's own transaction with
/// every refusal ahead of it, which is what makes this hold.
#[test]
fn a_refused_tick_writes_no_journal_row() {
    let fixture = Fixture::new();
    let _change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let error = fixture
        .checkpoint(
            run_id,
            session_id,
            OperationId::new(),
            TaskDone {
                number: "1.3".into(),
                verify_command: "cargo test -p worker".into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected another command to be refused");

    assert_eq!(error.code(), "verify_command_mismatch");
    assert_eq!(
        row_count(&fixture.raw(), "task_ticks"),
        0,
        "a refused tick is not something that happened"
    );
}

/// Nothing is unique on (`change_id`, number): a task closed twice is two
/// ticks.
/// The second run is a fact about the work as much as the first, and a journal
/// that kept only the latest would read afterwards exactly like one where the
/// task was proved once.
#[test]
fn closing_a_task_twice_keeps_both_ticks() {
    let fixture = Fixture::new();
    let _change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let first = format!("{EXPECTED}; finished in 0.42s\n");
    let second = format!("{EXPECTED}; finished in 0.31s\n");

    for output in [&first, &second] {
        fixture
            .checkpoint(
                run_id,
                session_id,
                OperationId::new(),
                TaskDone {
                    number: "1.3".into(),
                    verify_command: VERIFY.into(),
                    output: output.clone(),
                },
            )
            .expect("checkpoint");
    }

    assert_eq!(
        ticks(&fixture.raw()),
        vec![("1.3".to_owned(), first), ("1.3".to_owned(), second)],
        "both runs are on the record, in the order they happened"
    );
}

/// The hold a checkpoint takes on the task it names.
fn hold_of(
    fixture: &Fixture,
    change: ChangeId,
    number: &str,
) -> (String, Option<String>, Option<String>) {
    fixture
        .raw()
        .query_row(
            "SELECT status, claimed_by, lease_until FROM tasks
             WHERE change_id = ?1 AND number = ?2",
            rusqlite::params![change.to_string(), number],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("task row")
}

/// Taking, renewing and letting go, in one test because they share a plan.
#[test]
fn a_checkpoint_takes_renews_and_releases_a_task() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let (run_id, session_id) = fixture.bound_run();

    let naming = |task: &str| {
        Some(SpecBinding {
            change_id: None,
            current_task: Some(task.to_owned()),
        })
    };

    fixture
        .store
        .bind_spec(run_id, &naming("1.3: cap the loop").expect("binding"))
        .expect("claim by naming");

    let (status, held_by, until) = hold_of(&fixture, change, "1.3");
    assert_eq!(status, "running", "a task in hand is running");
    assert_eq!(
        held_by.as_deref(),
        Some(session_id.to_string().as_str()),
        "the claim belongs to the session that named it"
    );
    let first_lease = until.expect("a claim has an end");

    std::thread::sleep(std::time::Duration::from_millis(20));
    fixture
        .store
        .bind_spec(run_id, &naming("1.3: cap the loop").expect("binding"))
        .expect("renew");
    let (_, _, renewed) = hold_of(&fixture, change, "1.3");
    assert!(
        renewed.expect("still held") > first_lease,
        "a second checkpoint pushes the hold out"
    );

    // Prose naming no task claims nothing, and is not an error. `current_task`
    // is free text and carries work that is not a task at all.
    fixture
        .store
        .bind_spec(run_id, &naming("архивация").expect("binding"))
        .expect("prose is not an error");
    let (_, still_held, _) = hold_of(&fixture, change, "1.3");
    assert_eq!(
        still_held.as_deref(),
        Some(session_id.to_string().as_str()),
        "prose must not steal or drop a claim"
    );
}

// --- closing a task refused over a trespass its edits recorded --------------
//
// `append_ledger` stamps `file_ledger.trespass_on` when an edit lands on a
// path another live hold declares (`store_contract.rs`), and `close_task`
// (`store.rs`) refuses to let that collision be filed as a clean piece of
// work.

/// The absolute path a real harness hands the hook — not the relative one the
/// plan records — landing on the file task "4" declares.
const TRESPASS_EDIT: &str = "/tmp/project/src/store.rs";

/// Session A holding task 3 and session B holding task 4 of
/// `planned_change_for_trespass`, with session A's edit already recorded on
/// the file task 4 declares — the setup every trespass test in this file
/// shares. Both session ids travel: A is the one whose close the trespass
/// tests refuse, B is the one a clean close needs to prove task 4 is not
/// swept up in a collision that was never its own.
fn fixture_with_a_recorded_trespass() -> (Fixture, ChangeId, RunId, SessionId, SessionId) {
    let fixture = Fixture::new();
    let change = fixture.planned_change_for_trespass();
    let (run_id, session_a) = fixture.bound_run_unclaimed();
    fixture.hold(run_id, "3: cap the loop");

    let session_b = fixture.second_session(run_id, "harness-session-trespasser");
    fixture.hold(run_id, "4: cap the loop");

    assert_ne!(
        fixture.holder_of(change, "3"),
        fixture.holder_of(change, "4"),
        "second_session must actually win task 4's hold, or this exercises \
         self-trespass, not cross-session trespass"
    );

    fixture
        .store
        .append_ledger(
            run_id,
            session_a,
            &FileLedgerEntry {
                path: std::path::PathBuf::from(TRESPASS_EDIT),
                tool: "Edit".into(),
                observed_at: chrono::Utc::now(),
            },
        )
        .expect("append ledger");

    (fixture, change, run_id, session_a, session_b)
}

/// A tick that closes a task whose session trespassed onto a file another
/// live hold declares must be refused, not filed as evidence: the file was
/// not this task's alone to prove, and letting the close through would file
/// the collision away as if it never happened. Nothing about the refusal
/// leaves a mark: the task is left `running` with no evidence and no
/// `verified_at`, exactly as `a_tick_with_another_command_is_refused_and_leaves_the_task_open`
/// checks a verify-command refusal does, and the journal gains no row, exactly
/// as `a_refused_tick_writes_no_journal_row` checks for that refusal.
#[test]
fn a_task_that_took_another_agents_file_is_refused() {
    let (fixture, change, run_id, session_a, _session_b) = fixture_with_a_recorded_trespass();

    let error = fixture
        .checkpoint(
            run_id,
            session_a,
            OperationId::new(),
            TaskDone {
                number: "3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected the close to be refused over the recorded trespass");

    let StoreError::FileHeldByAnotherTask {
        number,
        path,
        holder,
    } = &error
    else {
        panic!("expected the trespass to be reported, got {error:?}");
    };
    assert_eq!(number, "3");
    assert_eq!(holder, "4");
    assert!(
        path.ends_with("src/store.rs"),
        "expected the trespassed file to be named, got {path:?}"
    );

    let (status, evidence, verified_at) = fixture.task_row(change, "3");
    assert_eq!(
        status, "running",
        "the task is left open, not closed over a trespass"
    );
    assert_eq!(
        evidence, None,
        "a refused close must not file the trespass as evidence"
    );
    assert_eq!(
        verified_at, None,
        "and it must not be recorded as verified either"
    );

    assert_eq!(
        row_count(&fixture.raw(), "task_ticks"),
        0,
        "a refused tick, over a trespass as over any other refusal, is not \
         something that happened"
    );
}

/// A count alone tells a reader nothing to act on; the file itself belongs in
/// the message. A separate test from the variant check above, so a Display
/// impl that drops the path would be caught even if the fields never move.
#[test]
fn a_refusal_over_a_held_file_names_the_file() {
    let (fixture, _change, run_id, session_a, _session_b) = fixture_with_a_recorded_trespass();

    let error = fixture
        .checkpoint(
            run_id,
            session_a,
            OperationId::new(),
            TaskDone {
                number: "3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected the close to be refused over the recorded trespass");

    assert!(
        error.to_string().contains("src/store.rs"),
        "the refusal should name the file it was refused over: {error}"
    );
}

/// The record was written while both holds were live; whether the other
/// agent later went quiet says nothing about whether the edits collided. So a
/// lapsed hold on the file task 4 declares must not excuse a trespass that
/// `append_ledger` already recorded while that hold was still in force.
#[test]
fn a_holder_that_lapsed_does_not_excuse_the_trespass() {
    let (fixture, change, run_id, session_a, _session_b) = fixture_with_a_recorded_trespass();

    // Scoped by change as well as by number, as `store_contract.rs`'s lapse
    // tests are: `tasks_number` is `UNIQUE(change_id, number)`
    // (`0009_tasks.sql`), not unique on the number alone.
    fixture
        .raw()
        .execute(
            "UPDATE tasks SET lease_until = ?1 WHERE number = ?2 AND change_id = ?3",
            rusqlite::params![
                (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
                "4",
                change.to_string(),
            ],
        )
        .expect("lapse the other holder's lease into the past");

    let error = fixture
        .checkpoint(
            run_id,
            session_a,
            OperationId::new(),
            TaskDone {
                number: "3".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("a holder going quiet since does not undo a recorded trespass");

    assert!(
        matches!(&error, StoreError::FileHeldByAnotherTask { number, .. } if number == "3"),
        "expected the trespass to still be reported, got {error:?}"
    );

    let (status, _, _) = fixture.task_row(change, "3");
    assert_eq!(
        status, "running",
        "the task is left open, not closed over a trespass"
    );
}

/// The trespass query in `close_task` (`store.rs`) is scoped by `t.number`,
/// so a collision recorded against task 3 must not reach task 4's own close —
/// the fourth scenario the requirement states. Task 4 never trespassed
/// anything; it is proven with its own plan-named command, and the fact that
/// an unrelated row in the same change's `file_ledger` carries a trespass
/// must not be visible to it at all.
#[test]
fn a_clean_close_is_not_refused_by_an_unrelated_trespass() {
    let (fixture, change, run_id, _session_a, session_b) = fixture_with_a_recorded_trespass();

    let result = fixture
        .checkpoint(
            run_id,
            session_b,
            OperationId::new(),
            TaskDone {
                number: "4".into(),
                verify_command: VERIFY.into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect("task 4's own close must not be refused by task 3's trespass");

    let closed = result.task.expect("the checkpoint closed a task");
    assert_eq!(closed.number, "4");

    let (status, evidence, verified_at) = fixture.task_row(change, "4");
    assert_eq!(status, "done", "task 4 closes on its own merits");
    assert!(evidence.is_some() && verified_at.is_some());
}

/// `close_task`'s doc comment (`store.rs`) states its refusals fire in one
/// order: the run's binding, then the slug, then the number, then the
/// command, then the ledger. A task that is both trespassing and ticking with
/// a command the plan never named must be reported for the command, not the
/// trespass — otherwise that ordering is a claim in prose nobody checks.
#[test]
fn a_wrong_command_is_reported_before_a_trespass() {
    let (fixture, change, run_id, session_a, _session_b) = fixture_with_a_recorded_trespass();

    let error = fixture
        .checkpoint(
            run_id,
            session_a,
            OperationId::new(),
            TaskDone {
                number: "3".into(),
                verify_command: "cargo test -p worker".into(),
                output: format!("{EXPECTED}\n"),
            },
        )
        .expect_err("expected the wrong command to be refused ahead of the trespass");

    assert!(
        matches!(&error, StoreError::VerifyCommandMismatch { number, expected }
            if number == "3" && expected == VERIFY),
        "the command check comes before the ledger check, so this refusal \
         must be about the command, got {error:?}"
    );

    let (status, _, _) = fixture.task_row(change, "3");
    assert_eq!(
        status, "running",
        "the task is left open, not closed over either refusal"
    );
}
