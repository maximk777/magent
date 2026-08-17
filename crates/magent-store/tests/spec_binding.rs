//! Binding a run to the spec it is executing.
//!
//! This is the one thing a spec-driven process cannot get from files alone. The
//! proposal and the task list live in `openspec/` and are the source of truth;
//! what they cannot say is which task is in flight right now, in this session,
//! after a compaction that erased the model's own memory of it.
//!
//! So the run holds a reference and nothing more. Copying the spec into the
//! database would create a second version of it that drifts, and the moment the
//! two disagree the wrong one is the one the agent trusts.

use magent_core::{
    CheckpointCommand, CheckpointOrigin, Classification, HarnessKind, OperationId, PlanCommand,
    ProposeCommand, SpecBinding, StartRunCommand, TaskDone, TaskDraft, WorkflowStage,
};
use magent_store::{FactContext, Store, StoreError};

/// The command the planned task below is verified by, quoted exactly where a
/// tick has to match it.
const VERIFY: &str = "cargo test -p worker retry";

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    root: std::path::PathBuf,
    context: FactContext,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("magent.db")).expect("open");
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir");
        let resolved = store.resolve_workspace_for(&root).expect("resolve");

        Self {
            dir,
            store,
            root,
            context: FactContext {
                workspace_id: Some(resolved.workspace_id),
                namespace: None,
                ..FactContext::default()
            },
        }
    }

    /// A change whose plan holds the given task numbers, for the ticks a refusal
    /// is provoked from. The numbers are the caller's because what a tick misses
    /// by is the point: a plan that has no task "1" refuses one differently from
    /// a plan that has.
    ///
    /// Proposed with `skip_specs`, which is what lets a change be planned
    /// straight out of `drafting`: nothing here is about the requirements, only
    /// about what a refused tick does to the run's binding.
    fn planned_change(&self, slug: &str, numbers: &[&str]) {
        let change = self
            .store
            .propose(
                &ProposeCommand {
                    operation_id: OperationId::new(),
                    slug: slug.into(),
                    title: format!("The change filed as {slug}"),
                    classification: Classification::Bounded,
                    why: "Retries currently have no ceiling and can loop forever.".into(),
                    what_changes: vec!["Add a configurable retry budget".into()],
                    capabilities: vec![],
                    impact: None,
                    skip_specs: true,
                },
                &self.context,
            )
            .expect("propose")
            .id;

        self.store
            .plan(
                &PlanCommand {
                    operation_id: OperationId::new(),
                    change,
                    tasks: numbers
                        .iter()
                        .map(|number| TaskDraft {
                            number: (*number).to_owned(),
                            title: format!("Cap the retry loop, step {number}"),
                            body: None,
                            files: vec!["crates/worker/src/retry.rs".into()],
                            consumes: None,
                            produces: None,
                            verify_command: VERIFY.into(),
                            expected_output: "test result: ok".into(),
                            covers: vec![],
                        })
                        .collect(),
                },
                &self.context,
            )
            .expect("plan");
    }

    fn start(&self, task: &str) -> (magent_core::RunId, magent_core::SessionId) {
        let started = self
            .store
            .start_run(
                &StartRunCommand {
                    operation_id: OperationId::new(),
                    task: task.into(),
                    resume_run_id: None,
                    external_session_hint: None,
                    workspace_roots: vec![self.root.clone()],
                },
                HarnessKind::ClaudeCode,
            )
            .expect("start");
        (started.run_id, started.session_id)
    }

    fn checkpoint(
        &self,
        run_id: magent_core::RunId,
        session_id: magent_core::SessionId,
        summary: &str,
    ) {
        self.send(run_id, session_id, OperationId::new(), summary, None, None)
            .expect("checkpoint");
    }

    /// A checkpoint with whatever a caller wants to hang on it, and the refusal
    /// handed back rather than unwrapped: what a refused checkpoint leaves
    /// behind is the thing under test below.
    ///
    /// The `operation_id` is the caller's, because a replay is the same command
    /// sent twice and a test has to be able to repeat one.
    fn send(
        &self,
        run_id: magent_core::RunId,
        session_id: magent_core::SessionId,
        operation_id: OperationId,
        summary: &str,
        task_done: Option<TaskDone>,
        binding: Option<SpecBinding>,
    ) -> Result<magent_core::CheckpointResult, StoreError> {
        self.store.save_checkpoint(&CheckpointCommand {
            operation_id,
            run_id,
            session_id,
            stage: WorkflowStage::Executing,
            origin: CheckpointOrigin::Enriched,
            completed_steps: vec![],
            next_steps: vec![],
            decisions: vec![],
            rejected: vec![],
            changed_files: vec![],
            verification: vec![],
            risks: vec![],
            handoff_summary: summary.into(),
            task_done,
            binding,
        })
    }
}

fn binding(change_id: &str, task: Option<&str>) -> SpecBinding {
    SpecBinding {
        change_id: Some(change_id.to_owned()),
        paths: vec![format!("openspec/changes/{change_id}/tasks.md")],
        current_task: task.map(ToOwned::to_owned),
    }
}

// --- the reference ----------------------------------------------------------

#[test]
fn a_bound_run_reports_which_change_it_is_executing() {
    let fixture = Fixture::new();
    let (run_id, _) = fixture.start("add a retry budget");

    fixture
        .store
        .bind_spec(
            run_id,
            &binding("add-retry-budget", Some("2: wire the budget")),
        )
        .expect("bind");

    let snapshot = fixture.store.snapshot(run_id).expect("snapshot");
    let spec = snapshot.spec.expect("a binding");
    assert_eq!(spec.change_id.as_deref(), Some("add-retry-budget"));
    assert_eq!(spec.current_task.as_deref(), Some("2: wire the budget"));
    assert_eq!(spec.paths, ["openspec/changes/add-retry-budget/tasks.md"]);
}

#[test]
fn an_unbound_run_has_no_binding_rather_than_an_empty_one() {
    let fixture = Fixture::new();
    let (run_id, _) = fixture.start("something ad hoc");

    assert!(
        fixture
            .store
            .snapshot(run_id)
            .expect("snapshot")
            .spec
            .is_none(),
        "plenty of work is not spec-driven, and an empty binding reads as a broken one"
    );
}

/// The binding is what survives a session. Losing it to a reopen would defeat
/// the whole point, since a fresh process is exactly when it is needed.
#[test]
fn the_binding_survives_reopening_the_store() {
    let fixture = Fixture::new();
    let (run_id, _) = fixture.start("add a retry budget");
    fixture
        .store
        .bind_spec(run_id, &binding("add-retry-budget", Some("2")))
        .expect("bind");

    // A second handle on the same file, which is what a fresh hook process is.
    let reopened = Store::open(&fixture.dir.path().join("magent.db")).expect("reopen");

    let spec = reopened
        .snapshot(run_id)
        .expect("snapshot")
        .spec
        .expect("binding");
    assert_eq!(spec.change_id.as_deref(), Some("add-retry-budget"));
}

// --- advancing --------------------------------------------------------------

/// Moving to the next task must not require restating the change. Making the
/// caller repeat it is how a run ends up half-bound.
#[test]
fn advancing_the_task_leaves_the_change_alone() {
    let fixture = Fixture::new();
    let (run_id, _) = fixture.start("add a retry budget");
    fixture
        .store
        .bind_spec(run_id, &binding("add-retry-budget", Some("1")))
        .expect("bind");

    fixture
        .store
        .bind_spec(
            run_id,
            &SpecBinding {
                change_id: None,
                paths: vec![],
                current_task: Some("2".into()),
            },
        )
        .expect("advance");

    let spec = fixture
        .store
        .snapshot(run_id)
        .expect("snapshot")
        .spec
        .expect("binding");
    assert_eq!(spec.change_id.as_deref(), Some("add-retry-budget"));
    assert_eq!(spec.current_task.as_deref(), Some("2"));
    assert_eq!(
        spec.paths,
        ["openspec/changes/add-retry-budget/tasks.md"],
        "the paths were not restated, so they were not dropped"
    );
}

/// A run that starts ad hoc and turns out to be a change is the normal way this
/// happens: the hook opened the run from the first prompt, before anyone knew.
#[test]
fn a_run_can_be_bound_after_it_has_already_started() {
    let fixture = Fixture::new();
    let (run_id, session_id) = fixture.start("look into the timeouts");
    fixture.checkpoint(run_id, session_id, "traced it to the retry loop");

    fixture
        .store
        .bind_spec(run_id, &binding("add-retry-budget", Some("1")))
        .expect("bind");

    let snapshot = fixture.store.snapshot(run_id).expect("snapshot");
    assert!(snapshot.spec.is_some());
    assert_eq!(
        snapshot
            .latest_checkpoint
            .expect("checkpoint")
            .handoff_summary,
        "traced it to the retry loop",
        "binding a spec is not a reason to lose what was already recorded"
    );
}

/// A binding a checkpoint carries belongs to that checkpoint's operation, so a
/// checkpoint the store refuses leaves the binding exactly where it was.
///
/// Three refused messages: one carrying no binding, one whose binding would have
/// moved the run to another task, and one whose binding would have rebound it to
/// a second live change. The last two are what tell "inside the operation" apart
/// from "merely earlier in it" — a run left pointing somewhere by a message that
/// was rejected would send the next session to the wrong task, with nothing
/// recorded to explain why.
#[test]
fn a_refused_checkpoint_leaves_the_binding_as_it_was() {
    let fixture = Fixture::new();
    fixture.planned_change("add-retry-budget", &["1"]);
    // A real second change, whose plan has no task "1" — so a message that
    // rebinds to it and ticks "1" is refused for the number rather than for the
    // slug, which is what a run rebound to a live change actually meets.
    fixture.planned_change("spend-the-budget", &["7", "8"]);
    let (run_id, session_id) = fixture.start("cap the retry loop");
    fixture
        .store
        .bind_spec(
            run_id,
            &binding("add-retry-budget", Some("1: cap the loop")),
        )
        .expect("bind");

    // A command the plan did not name, which `close_task` refuses once it has
    // found the task — the refusal a correctly numbered tick still meets.
    let wrong_command = || TaskDone {
        number: "1".into(),
        verify_command: "cargo test -p worker".into(),
        output: "test result: ok\n".into(),
    };

    let refused = fixture
        .send(
            run_id,
            session_id,
            OperationId::new(),
            "capped the loop",
            Some(wrong_command()),
            None,
        )
        .expect_err("expected the tick's command to be refused");
    assert_eq!(refused.code(), "verify_command_mismatch");

    let refused = fixture
        .send(
            run_id,
            session_id,
            OperationId::new(),
            "capped the loop",
            Some(wrong_command()),
            Some(SpecBinding {
                change_id: None,
                paths: vec!["openspec/changes/add-retry-budget/proposal.md".into()],
                current_task: Some("4: something else entirely".into()),
            }),
        )
        .expect_err("expected the tick's command to be refused here too");
    assert_eq!(
        refused.code(),
        "verify_command_mismatch",
        "the binding this message carried named no other change, so the tick was read \
         against the same plan"
    );

    // The third names the other change, and meets a different refusal: the tick
    // resolves against the binding this very message supplied, which is the
    // ordering `one_message_binds_the_run_and_closes_its_first_task` is about —
    // and the slug still has to go back with it.
    let refused = fixture
        .send(
            run_id,
            session_id,
            OperationId::new(),
            "capped the loop",
            Some(wrong_command()),
            Some(binding("spend-the-budget", Some("7: spend it"))),
        )
        .expect_err("expected a tick on a number the rebound plan has not to be refused");
    let StoreError::TaskNotFound { slug, number, open } = &refused else {
        panic!("expected the number to be reported as unknown, got {refused:?}");
    };
    assert_eq!(
        (slug.as_str(), number.as_str()),
        ("spend-the-budget", "1"),
        "the tick was read against the plan this message bound the run to"
    );
    assert_eq!(
        open,
        &["7".to_owned(), "8".to_owned()],
        "and the numbers it does have came back with the refusal"
    );

    assert_eq!(
        fixture.store.checkpoint_count(run_id).expect("count"),
        0,
        "none of the refused checkpoints was recorded"
    );

    let spec = fixture
        .store
        .snapshot(run_id)
        .expect("snapshot")
        .spec
        .expect("the binding it had before");
    assert_eq!(
        spec.change_id.as_deref(),
        Some("add-retry-budget"),
        "the change the third message named went back with it"
    );
    assert_eq!(
        spec.current_task.as_deref(),
        Some("1: cap the loop"),
        "and so did the task the second named"
    );
    assert_eq!(
        spec.paths,
        ["openspec/changes/add-retry-budget/tasks.md"],
        "and the path it added"
    );
}

/// A retry reaches the store as the same command under the same `operation_id`,
/// and is answered from the recorded response rather than run again — the binding
/// included, because it is now part of that operation rather than a write that
/// followed it.
///
/// What it costs to get this wrong is not a duplicate row but a wrong one: the
/// run has moved on since, so a binding re-applied on the replay would drag it
/// back to the task the retried message named, and the session that resumes from
/// it would pick up the task before the one in hand.
#[test]
fn a_replayed_checkpoint_does_not_re_apply_its_binding() {
    let fixture = Fixture::new();
    let (run_id, session_id) = fixture.start("add a retry budget");

    let operation_id = OperationId::new();
    let first = fixture
        .send(
            run_id,
            session_id,
            operation_id,
            "started on the first task",
            None,
            Some(binding("add-retry-budget", Some("1: cap the loop"))),
        )
        .expect("checkpoint");

    // Whatever happened in between: this agent finished the task and moved on,
    // or another one did, while the first call's answer was still in flight.
    fixture
        .store
        .bind_spec(
            run_id,
            &SpecBinding {
                change_id: None,
                paths: vec![],
                current_task: Some("2: spend the budget".into()),
            },
        )
        .expect("advance");

    let replayed = fixture
        .send(
            run_id,
            session_id,
            operation_id,
            "started on the first task",
            None,
            Some(binding("add-retry-budget", Some("1: cap the loop"))),
        )
        .expect("replay");

    assert_eq!(
        first, replayed,
        "the replay answers what the first call did"
    );
    assert_eq!(
        fixture.store.checkpoint_count(run_id).expect("count"),
        1,
        "and records nothing a second time"
    );

    let spec = fixture
        .store
        .snapshot(run_id)
        .expect("snapshot")
        .spec
        .expect("binding");
    assert_eq!(
        spec.current_task.as_deref(),
        Some("2: spend the budget"),
        "the replay must not have put the run back on the task it named"
    );
    assert_eq!(spec.change_id.as_deref(), Some("add-retry-budget"));
}

/// Nothing here validates that the file exists. The spec lives in git and this
/// is a reference to it; a run bound to a path on a branch someone else has is
/// still correctly bound, and refusing it would make the reference useless.
#[test]
fn a_path_that_does_not_exist_yet_is_still_a_valid_reference() {
    let fixture = Fixture::new();
    let (run_id, _) = fixture.start("add a retry budget");

    fixture
        .store
        .bind_spec(run_id, &binding("not-written-yet", None))
        .expect("bind");

    let spec = fixture
        .store
        .snapshot(run_id)
        .expect("snapshot")
        .spec
        .expect("binding");
    assert_eq!(spec.change_id.as_deref(), Some("not-written-yet"));
    assert!(spec.current_task.is_none());
}
