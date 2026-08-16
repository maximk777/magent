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
    CheckpointCommand, CheckpointOrigin, HarnessKind, OperationId, SpecBinding, StartRunCommand,
    WorkflowStage,
};
use magent_store::Store;

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("magent.db")).expect("open");
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir");

        Self { dir, store, root }
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
        self.store
            .save_checkpoint(&CheckpointCommand {
                operation_id: OperationId::new(),
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
            })
            .expect("checkpoint");
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
