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
//! `expected_output` is only *reported* on. That string is written before the
//! work is done, so refusing a tick over it would stop correct work.

use magent_core::{
    ChangeId, CheckpointCommand, CheckpointOrigin, CheckpointResult, Classification, DeltaOp,
    HarnessKind, OperationId, PlanCommand, ProposeCommand, RequirementDraft, RunId, ScenarioDraft,
    SessionId, SpecBinding, SpecifyCommand, StartRunCommand, TaskDone, TaskDraft, WorkflowStage,
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
                        requirement_id: None,
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

        self.store
            .plan(
                &PlanCommand {
                    operation_id: OperationId::new(),
                    change,
                    tasks: vec![task("1.3", &[REQUIREMENT]), task("2", &[])],
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
                    paths: Vec::new(),
                    current_task: Some("1.3: cap the loop".into()),
                },
            )
            .expect("bind");

        (run_id, session_id)
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
            binding: None,
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

fn task(number: &str, covers: &[&str]) -> TaskDraft {
    TaskDraft {
        number: number.into(),
        title: format!("Cap the retry loop, step {number}"),
        body: Some("Read the budget from config and stop once it is spent.".into()),
        files: vec!["crates/worker/src/retry.rs".into()],
        consumes: None,
        produces: Some("fn spend_budget(&mut self) -> bool".into()),
        verify_command: VERIFY.into(),
        expected_output: EXPECTED.into(),
        covers: covers.iter().map(|name| (*name).to_string()).collect(),
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
        closed.expected_output_found,
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
    assert!(
        !closed.expected_output_found,
        "{EXPECTED:?} does not appear in {output:?}"
    );

    let (status, evidence, verified_at) = fixture.task_row(change, "1.3");
    assert_eq!(status, "done", "the task closes either way");
    assert_eq!(evidence.as_deref(), Some(output));
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
    assert_eq!(status, "pending", "and the task is left as it was");
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
    assert_eq!(status, "pending", "the task is left open for the real run");
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
                paths: Vec::new(),
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
        status, "pending",
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
        status, "pending",
        "neither plan is closed while it is unclear which one this is"
    );
}
