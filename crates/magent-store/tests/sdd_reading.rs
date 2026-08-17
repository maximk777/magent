//! Reading the live specification back out of the store.
//!
//! Archiving a change folds its deltas into `capabilities`, `requirements` and
//! `scenarios` — and until these two methods existed, nothing read those tables
//! again. The whole point of keeping the specification as rows is that the next
//! session can ask what is currently true before proposing anything, so this
//! carries a change all the way through the process and then reads the result
//! back the way that session would.

use magent_core::{
    ArchiveCommand, ChangeId, CheckpointCommand, CheckpointOrigin, Classification, DeltaOp,
    HarnessKind, OperationId, PlanCommand, ProposeCommand, RequirementDraft, ScenarioDraft,
    SpecBinding, SpecifyCommand, StartRunCommand, TaskDone, TaskDraft, WorkflowStage,
};
use magent_store::{FactContext, Store};
use rusqlite::Connection;

const SLUG: &str = "add-retry-budget";
const RETIRE_SLUG: &str = "retire-the-per-attempt-rule";
const CAPABILITY: &str = "worker/retry";

/// Two requirements whose names sort the other way round from the order they
/// are specified in, so the assertions below pin the order `capability_detail`
/// documents rather than the order this test happens to write them in.
const BUDGET: &str = "The worker stops retrying once its budget is spent";
const ATTEMPT: &str = "Each attempt spends one unit of the budget";

const BUDGET_TEXT: &str = "The worker SHALL stop retrying once the budget is spent.";
const ATTEMPT_TEXT: &str = "The worker SHALL spend one unit of the budget per attempt.";

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

    /// A change carried the whole way: proposed, specified with two additions
    /// against one capability, planned, its task closed by a checkpoint
    /// carrying the evidence, and archived.
    ///
    /// The tick goes through `save_checkpoint` rather than an `UPDATE` on
    /// `tasks`, because that is the only thing that moves a change to `ready`
    /// and `archive` refuses one whose tasks are still open.
    fn archived_change(&self) -> ChangeId {
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
                    requirements: vec![
                        RequirementDraft {
                            op: DeltaOp::Added,
                            name: BUDGET.into(),
                            text: Some(BUDGET_TEXT.into()),
                            rename_to: None,
                            reason: None,
                            migration: None,
                            requirement_id: None,
                            // Two scenarios whose names also sort the other way
                            // round from their sequence, so an `ORDER BY name`
                            // here would swap them visibly.
                            scenarios: vec![
                                ScenarioDraft {
                                    name: "budget exhausted".into(),
                                    given: None,
                                    when: "the budget is exhausted".into(),
                                    then: "the job is parked".into(),
                                },
                                ScenarioDraft {
                                    name: "a fresh job".into(),
                                    given: Some("a job with a full budget".into()),
                                    when: "an attempt fails".into(),
                                    then: "the worker retries".into(),
                                },
                            ],
                        },
                        RequirementDraft {
                            op: DeltaOp::Added,
                            name: ATTEMPT.into(),
                            text: Some(ATTEMPT_TEXT.into()),
                            rename_to: None,
                            reason: None,
                            migration: None,
                            requirement_id: None,
                            scenarios: vec![ScenarioDraft {
                                name: "one attempt".into(),
                                given: None,
                                when: "an attempt is made".into(),
                                then: "one unit is spent".into(),
                            }],
                        },
                    ],
                },
                &self.context,
            )
            .expect("specify");

        self.store
            .plan(
                &PlanCommand {
                    operation_id: OperationId::new(),
                    change,
                    tasks: vec![task("Cap the retry loop", &[BUDGET, ATTEMPT])],
                },
                &self.context,
            )
            .expect("plan");

        self.close_the_task(SLUG);

        self.store
            .archive(
                &ArchiveCommand {
                    operation_id: OperationId::new(),
                    change,
                },
                &self.context,
            )
            .expect("archive");

        change
    }

    /// The plan's one task, closed by a checkpoint of a run bound to the
    /// change by its slug — which is what `runs.spec_change_id` holds.
    fn close_the_task(&self, slug: &str) {
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

        self.store
            .bind_spec(
                started.run_id,
                &SpecBinding {
                    change_id: Some(slug.into()),
                    paths: Vec::new(),
                    current_task: Some("1: the change's only task".into()),
                },
            )
            .expect("bind");

        let closed = self
            .store
            .save_checkpoint(&CheckpointCommand {
                operation_id: OperationId::new(),
                run_id: started.run_id,
                session_id: started.session_id,
                stage: WorkflowStage::Executing,
                origin: CheckpointOrigin::Enriched,
                completed_steps: vec!["did the change's one task".into()],
                next_steps: vec![],
                decisions: vec![],
                rejected: vec![],
                changed_files: vec![],
                verification: vec![],
                risks: vec![],
                handoff_summary: "The verification ran and printed what the plan expected.".into(),
                task_done: Some(TaskDone {
                    number: "1".into(),
                    verify_command: VERIFY.into(),
                    output: format!("running 3 tests\n...\n{EXPECTED}; finished in 0.42s\n"),
                }),
                binding: None,
            })
            .expect("checkpoint")
            .task
            .expect("the checkpoint closed a task");

        assert!(
            closed.change_ready,
            "the plan's only task is closed, so the change is ready to archive"
        );
    }

    /// A second change, taken the same way to `archived`, that retires one
    /// requirement of the live capability.
    ///
    /// This is what `requirements.status` exists for: `apply_delta` sets the row
    /// to `removed` and leaves it, and its scenarios, exactly where they are —
    /// `0007_sdd.sql` keeps them as the record of the decision. Nothing but the
    /// `status = 'live'` filter separates a retired requirement from a live one.
    fn retire(&self, name: &str) {
        let requirement_id = self.live_requirement_id(name);

        let change = self
            .store
            .propose(
                &ProposeCommand {
                    operation_id: OperationId::new(),
                    slug: RETIRE_SLUG.into(),
                    title: "Retire the per-attempt rule".into(),
                    classification: Classification::Bounded,
                    why: "The budget subsumes counting every attempt separately.".into(),
                    what_changes: vec!["Drop the per-attempt requirement".into()],
                    capabilities: vec![CAPABILITY.into()],
                    impact: None,
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
                    // No purpose: the capability is live by now, and `specify`
                    // refuses one it is not being asked to create.
                    purpose: None,
                    requirements: vec![RequirementDraft {
                        op: DeltaOp::Removed,
                        name: name.into(),
                        text: None,
                        rename_to: None,
                        reason: Some("The budget subsumes it.".into()),
                        migration: Some("Set the budget to one for the old behaviour.".into()),
                        requirement_id: Some(requirement_id),
                        scenarios: Vec::new(),
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
                    tasks: vec![task("Drop the per-attempt rule", &[name])],
                },
                &self.context,
            )
            .expect("plan");

        self.close_the_task(RETIRE_SLUG);

        self.store
            .archive(
                &ArchiveCommand {
                    operation_id: OperationId::new(),
                    change,
                },
                &self.context,
            )
            .expect("archive");
    }

    /// The id of a live requirement, read straight off the row.
    ///
    /// Off the row because there is nowhere else to read it from:
    /// `CapabilityDetail` carries no ids, so a change meaning to modify, remove
    /// or rename a requirement — all three of which `magent-core` refuses
    /// without one — cannot address it from anything these two methods return.
    /// Worth closing somewhere, but not here.
    fn live_requirement_id(&self, name: &str) -> String {
        self.raw()
            .query_row(
                "SELECT id FROM requirements WHERE name = ?1 AND status = 'live'",
                [name],
                |row| row.get(0),
            )
            .expect("live requirement id")
    }

    /// A second connection to the same file, for the assertions the store's own
    /// reading cannot make.
    fn raw(&self) -> Connection {
        Connection::open(&self.path).expect("raw connection")
    }
}

/// The one task of a change's plan, covering the requirements it names.
fn task(title: &str, covers: &[&str]) -> TaskDraft {
    TaskDraft {
        number: "1".into(),
        title: title.into(),
        body: Some("Read the budget from config and spend it per attempt.".into()),
        files: vec!["crates/worker/src/retry.rs".into()],
        consumes: None,
        produces: Some("fn spend_budget(&mut self) -> bool".into()),
        verify_command: VERIFY.into(),
        expected_output: EXPECTED.into(),
        covers: covers.iter().map(|name| (*name).to_string()).collect(),
    }
}

/// The step everything before it exists for: once a change is archived, what it
/// proposed *is* the specification, and a session that never saw the change can
/// read it.
#[test]
fn an_archived_change_shows_up_as_live_specification() {
    let fixture = Fixture::new();
    fixture.archived_change();

    let capabilities = fixture
        .store
        .live_capabilities(&fixture.context)
        .expect("live capabilities");

    assert_eq!(
        capabilities.len(),
        1,
        "one capability was created, so the index has one row: {capabilities:?}"
    );
    let capability = &capabilities[0];
    assert_eq!(capability.path, CAPABILITY);
    assert_eq!(capability.purpose, PURPOSE);
    assert_eq!(
        capability.requirement_count, 2,
        "both additions are live under it"
    );

    let detail = fixture
        .store
        .capability_detail(CAPABILITY, &fixture.context)
        .expect("capability detail")
        .expect("the capability is this workspace's");

    assert_eq!(detail.path, CAPABILITY);
    assert_eq!(detail.purpose, PURPOSE);

    // Both requirements were folded in by one `archive`, so they share a
    // `created_at` and `name` separates them — lexicographically, which puts
    // the one specified second first.
    let names: Vec<&str> = detail
        .requirements
        .iter()
        .map(|requirement| requirement.name.as_str())
        .collect();
    assert_eq!(names, vec![ATTEMPT, BUDGET]);

    let attempt = &detail.requirements[0];
    assert_eq!(attempt.text, ATTEMPT_TEXT);
    assert_eq!(attempt.scenarios.len(), 1);
    assert_eq!(attempt.scenarios[0].name, "one attempt");

    let budget = &detail.requirements[1];
    assert_eq!(budget.text, BUDGET_TEXT);
    let scenarios: Vec<(&str, Option<&str>, &str, &str)> = budget
        .scenarios
        .iter()
        .map(|scenario| {
            (
                scenario.name.as_str(),
                scenario.given.as_deref(),
                scenario.when.as_str(),
                scenario.then.as_str(),
            )
        })
        .collect();
    assert_eq!(
        scenarios,
        vec![
            (
                "budget exhausted",
                None,
                "the budget is exhausted",
                "the job is parked"
            ),
            (
                "a fresh job",
                Some("a job with a full budget"),
                "an attempt fails",
                "the worker retries"
            ),
        ],
        "the scenarios come back in the sequence the change wrote them, \
         with `given` where there was one"
    );
}

/// A requirement a later change retired is that change's history, not the
/// product's specification — and nothing deletes it, so the `status = 'live'`
/// filter is the only thing keeping it out of either answer.
#[test]
fn a_retired_requirement_leaves_the_live_specification() {
    let fixture = Fixture::new();
    fixture.archived_change();
    fixture.retire(ATTEMPT);

    let retired: i64 = fixture
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM requirements WHERE name = ?1 AND status = 'removed'",
            [ATTEMPT],
            |row| row.get(0),
        )
        .expect("retired requirement");
    assert_eq!(
        retired, 1,
        "the row is still there, retired — otherwise the filter below proves nothing"
    );

    let capabilities = fixture
        .store
        .live_capabilities(&fixture.context)
        .expect("live capabilities");

    assert_eq!(capabilities.len(), 1, "the capability itself stays");
    assert_eq!(
        capabilities[0].requirement_count, 1,
        "one of the two requirements was retired"
    );

    let detail = fixture
        .store
        .capability_detail(CAPABILITY, &fixture.context)
        .expect("capability detail")
        .expect("the capability is this workspace's");

    let names: Vec<&str> = detail
        .requirements
        .iter()
        .map(|requirement| requirement.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![BUDGET],
        "only the surviving requirement is specification"
    );
    assert_eq!(
        detail.requirements[0].scenarios.len(),
        2,
        "the survivor keeps its own scenarios, not the retired one's"
    );
}

/// Asked about a capability nothing has archived, the store says so rather than
/// refusing: the caller can then answer with the index, which is the courtesy
/// `change_detail` already extends to an id it does not know.
#[test]
fn an_unknown_capability_reads_as_none() {
    let fixture = Fixture::new();
    fixture.archived_change();

    let detail = fixture
        .store
        .capability_detail("worker/backoff", &fixture.context)
        .expect("capability detail");

    assert!(
        detail.is_none(),
        "nothing has archived worker/backoff: {detail:?}"
    );
}
