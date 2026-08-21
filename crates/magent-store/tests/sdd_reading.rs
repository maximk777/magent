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
    SpecBinding, SpecifyCommand, StartRunCommand, TaskClosed, TaskDone, TaskDraft, WorkflowStage,
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

/// What the plan tells the agent executing its one task — the fields that exist
/// precisely because that agent sees its own row and nothing around it.
const BODY: &str = "Read the budget from config and spend it per attempt.";
const CONSUMES: &str = "struct RetryConfig, from the task before this one";
const PRODUCES: &str = "fn spend_budget(&mut self) -> bool";

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
        let change = self.planned_change();

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

    /// The same change stopped at `planned`: its one task is written and still
    /// `pending`, which is the state the agent about to execute it reads it in.
    fn planned_change(&self) -> ChangeId {
        let change = self.specified_change();

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

        change
    }

    /// The same change stopped at `specified`: proposed, and its two additions
    /// attached. This is the state a spec review reads it in — the deltas are
    /// written, and nothing has folded them into the live base yet.
    fn specified_change(&self) -> ChangeId {
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

        change
    }

    /// The plan's one task, closed and leaving the change ready to archive.
    fn close_the_task(&self, slug: &str) {
        let closed = self.tick(slug, "1");

        assert!(
            closed.change_ready,
            "the plan's only task is closed, so the change is ready to archive"
        );
    }

    /// Task `number` of the plan, closed by a checkpoint of a run bound to the
    /// change by its slug — which is what `runs.spec_change_id` holds.
    ///
    /// Hands the result back rather than asserting on it, because a plan with a
    /// task still open closes one without becoming ready, and that is a state a
    /// test needs: a `ready` change refuses a replan.
    fn tick(&self, slug: &str, number: &str) -> TaskClosed {
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
                    current_task: Some(format!("{number}: the task in hand")),
                },
            )
            .expect("bind");

        self.store
            .save_checkpoint(&CheckpointCommand {
                operation_id: OperationId::new(),
                run_id: started.run_id,
                session_id: started.session_id,
                stage: WorkflowStage::Executing,
                origin: CheckpointOrigin::Enriched,
                completed_steps: vec!["did the task in hand".into()],
                next_steps: vec![],
                decisions: vec![],
                rejected: vec![],
                changed_files: vec![],
                verification: vec![],
                risks: vec![],
                handoff_summary: "The verification ran and printed what the plan expected.".into(),
                task_done: Some(TaskDone {
                    number: number.into(),
                    verify_command: VERIFY.into(),
                    output: format!("running 3 tests\n...\n{EXPECTED}; finished in 0.42s\n"),
                }),
                binding: None,
            })
            .expect("checkpoint")
            .task
            .expect("the checkpoint closed a task")
    }

    /// A second change, taken the same way to `archived`, that retires one
    /// requirement of the live capability.
    ///
    /// This is what `requirements.status` exists for: `apply_delta` sets the row
    /// to `removed` and leaves it, and its scenarios, exactly where they are —
    /// `0007_sdd.sql` keeps them as the record of the decision. Nothing but the
    /// `status = 'live'` filter separates a retired requirement from a live one.
    ///
    /// Hands the change back so a test can read the removal delta itself, not
    /// only its effect on the live base.
    fn retire(&self, name: &str) -> ChangeId {
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

        change
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
        body: Some(BODY.into()),
        // Two, because one file reads the same whether the loader returns the
        // list or only its first element.
        files: vec![
            "crates/worker/src/retry.rs".into(),
            "crates/worker/src/config.rs".into(),
        ],
        consumes: Some(CONSUMES.into()),
        produces: Some(PRODUCES.into()),
        verify_command: VERIFY.into(),
        expected_output: vec![EXPECTED.into()],
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

/// A reader meets a requirement while deciding what to propose against it, and
/// what they want is why it says what it says. The change that wrote it is the
/// shortest way to that, and until now the row did not record one.
#[test]
fn a_live_requirement_names_the_change_that_last_wrote_it() {
    let fixture = Fixture::new();
    fixture.archived_change();

    let detail = fixture
        .store
        .capability_detail(CAPABILITY, &fixture.context)
        .expect("capability detail")
        .expect("the capability is this workspace's");

    let requirement = detail
        .requirements
        .iter()
        .find(|requirement| requirement.name == BUDGET)
        .expect("the budget requirement is live");

    let origin = requirement
        .origin
        .as_ref()
        .expect("a requirement archived under a change names that change");

    assert_eq!(origin.slug, SLUG);
    assert_eq!(origin.title, "Add a retry budget");
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

/// The gap writing this change's own proposal ran into: a reviewer dispatched
/// to check a change reads it through `change_detail`, and until now that
/// answered with the names of the requirements and nothing else — so the review
/// was of a list of titles, against a specification the reviewer could not see.
#[test]
fn a_specified_change_reads_back_its_requirements() {
    let fixture = Fixture::new();
    let change = fixture.specified_change();

    let detail = fixture
        .store
        .change_detail(change, &fixture.context)
        .expect("change detail")
        .expect("the change is this workspace's");

    assert_eq!(detail.deltas.len(), 2, "{:?}", detail.deltas);

    // `created_at` then `name`, the order `load_deltas` applies them in, which
    // puts the requirement specified second first.
    let attempt = &detail.deltas[0];
    assert_eq!(attempt.name, ATTEMPT);
    assert_eq!(attempt.op, DeltaOp::Added);
    assert_eq!(
        attempt.text.as_deref(),
        Some(ATTEMPT_TEXT),
        "the name without the text is not something anyone can review"
    );
    assert_eq!(attempt.rename_to, None);
    assert_eq!(attempt.reason, None);
    assert_eq!(attempt.migration, None);

    let budget = &detail.deltas[1];
    assert_eq!(budget.name, BUDGET);
    assert_eq!(budget.text.as_deref(), Some(BUDGET_TEXT));

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
        "a delta's scenarios come back in the sequence it wrote them, with \
         `given` where there was one"
    );
    assert_eq!(
        attempt.scenarios.len(),
        1,
        "each delta gets its own scenarios, not the other's: {:?}",
        attempt.scenarios
    );
}

/// A removal carries no text and never did — what it carries is why it is going
/// and what to do instead, and those are the two things a reviewer of a removal
/// has to read. A null `text` here is the honest answer, not a gap to fill in.
#[test]
fn a_removed_delta_reads_back_its_reason() {
    let fixture = Fixture::new();
    fixture.archived_change();
    let change = fixture.retire(ATTEMPT);

    let detail = fixture
        .store
        .change_detail(change, &fixture.context)
        .expect("change detail")
        .expect("the change is this workspace's");

    assert_eq!(detail.deltas.len(), 1, "{:?}", detail.deltas);
    let removal = &detail.deltas[0];

    assert_eq!(removal.op, DeltaOp::Removed);
    assert_eq!(removal.name, ATTEMPT);
    assert_eq!(
        removal.text, None,
        "a removal proposes no text, and inventing one would be a lie"
    );
    assert_eq!(removal.reason.as_deref(), Some("The budget subsumes it."));
    assert_eq!(
        removal.migration.as_deref(),
        Some("Set the budget to one for the old behaviour.")
    );
    assert!(
        removal.scenarios.is_empty(),
        "a removal writes no scenarios: {:?}",
        removal.scenarios
    );
}

/// `TaskDraft.body`, `consumes` and `produces` exist because the agent
/// executing one task sees its own row and nothing around it — and until this
/// read them back, the only way that agent could be handed them was for an
/// orchestrator to hold them in context, which is exactly what a compaction
/// takes away.
#[test]
fn a_planned_task_reads_back_whole() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();

    let detail = fixture
        .store
        .change_detail(change, &fixture.context)
        .expect("change detail")
        .expect("the change is this workspace's");

    assert_eq!(detail.tasks.len(), 1, "{:?}", detail.tasks);
    let task = &detail.tasks[0];

    assert_eq!(task.number, "1");
    assert_eq!(task.title, "Cap the retry loop");
    assert_eq!(task.status, "pending");
    assert_eq!(task.body.as_deref(), Some(BODY));
    assert_eq!(
        task.files,
        vec![
            "crates/worker/src/retry.rs".to_string(),
            "crates/worker/src/config.rs".to_string()
        ],
        "the files come back as the list the plan wrote, not as the JSON they are stored in"
    );
    assert_eq!(task.consumes.as_deref(), Some(CONSUMES));
    assert_eq!(task.produces.as_deref(), Some(PRODUCES));
    assert_eq!(task.verify_command, VERIFY);
    assert_eq!(
        task.expected_output,
        vec![EXPECTED.to_string()],
        "the command without what it should print is half an instruction"
    );

    assert_eq!(
        task.evidence, None,
        "nothing has been run against this task yet"
    );
    assert_eq!(task.verified_at, None);
}

/// A tick is worth no more than the evidence under it, and evidence nobody can
/// read is a checked box. Reading a closed task back with what its command
/// printed, and when, is what makes auditing one possible instead of trusting
/// the agent that ticked it.
#[test]
fn a_closed_task_carries_its_evidence() {
    let fixture = Fixture::new();
    let change = fixture.archived_change();

    let detail = fixture
        .store
        .change_detail(change, &fixture.context)
        .expect("change detail")
        .expect("the change is this workspace's");

    let task = &detail.tasks[0];
    assert_eq!(task.status, "done");

    let evidence = task.evidence.as_deref().expect("the tick carried output");
    assert!(
        evidence.contains(EXPECTED),
        "the output is kept as it came, not summarised: {evidence}"
    );
    assert!(
        task.verified_at.is_some(),
        "evidence and the moment it was taken land together"
    );
}

/// Every close of a task appends a row to `task_ticks`, and until this read them
/// back nothing did: a journal nobody can read is a checked box with extra
/// steps. What it answers that `TaskSummary.evidence` cannot is what was run
/// against which plan — evidence sits on the task row and the next close
/// overwrites it, while a tick is one run and stays one run.
#[test]
fn a_change_reads_back_its_ticks() {
    let fixture = Fixture::new();
    let change = fixture.archived_change();

    let detail = fixture
        .store
        .change_detail(change, &fixture.context)
        .expect("change detail")
        .expect("the change is this workspace's");

    assert_eq!(detail.ticks.len(), 1, "{:?}", detail.ticks);
    let tick = &detail.ticks[0];

    assert_eq!(tick.number, "1");
    assert_eq!(tick.verify_command, VERIFY);
    assert!(
        tick.output.contains(EXPECTED),
        "the output is kept as it came, not summarised: {}",
        tick.output
    );
    assert!(
        tick.expected_output_missing.is_empty(),
        "the command printed every marker the plan named: {:?}",
        tick.expected_output_missing
    );
    assert!(
        tick.in_current_plan,
        "task 1 is still the task the plan holds"
    );
}

/// Why the journal is keyed to the change and not to the task row: `Store::plan`
/// deletes a change's tasks wholesale — that is what replanning means — and
/// nothing that happened stops having happened because the plan changed.
///
/// `in_current_plan` is what tells this case apart from the one above. Without
/// it a tick against a number no task holds reads exactly like a tick against a
/// task the plan still has.
///
/// The plan is written with a second task and the change is left short of
/// `archived`, because a replan has to be legal for there to be anything to
/// prove: `mark_change_ready` moves a change with nothing open to `ready`, and
/// `require_plannable_change` refuses `ready` and `archived` alike. The same
/// shape the sibling test in `task_closing.rs` uses, for the same reason.
#[test]
fn a_tick_survives_the_plan_it_was_made_against() {
    let fixture = Fixture::new();
    let change = fixture.specified_change();

    fixture
        .store
        .plan(
            &PlanCommand {
                operation_id: OperationId::new(),
                change,
                tasks: vec![
                    task("Cap the retry loop", &[BUDGET, ATTEMPT]),
                    TaskDraft {
                        number: "2".into(),
                        ..task("Wire the budget into the config", &[])
                    },
                ],
            },
            &fixture.context,
        )
        .expect("plan");

    fixture.tick(SLUG, "1");

    fixture
        .store
        .plan(
            &PlanCommand {
                operation_id: OperationId::new(),
                change,
                tasks: vec![TaskDraft {
                    number: "9".into(),
                    ..task("Cap the retry loop", &[BUDGET, ATTEMPT])
                }],
            },
            &fixture.context,
        )
        .expect("replan");

    let detail = fixture
        .store
        .change_detail(change, &fixture.context)
        .expect("change detail")
        .expect("the change is this workspace's");

    assert_eq!(
        detail
            .tasks
            .iter()
            .map(|task| task.number.as_str())
            .collect::<Vec<_>>(),
        vec!["9"],
        "the replan replaced the plan, the row task 1 was closed on included"
    );

    assert_eq!(detail.ticks.len(), 1, "{:?}", detail.ticks);
    let tick = &detail.ticks[0];

    assert_eq!(tick.number, "1");
    assert!(
        !tick.in_current_plan,
        "no task numbered 1 is left, so the tick records a plan this change no longer has"
    );
}

/// A change nothing has been run against reads back with an empty journal rather
/// than with the field absent: a caller that had to tell "no ticks" apart from
/// "ticks not loaded" would be guessing at the difference.
#[test]
fn a_change_with_no_ticks_reads_an_empty_journal() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();

    let detail = fixture
        .store
        .change_detail(change, &fixture.context)
        .expect("change detail")
        .expect("the change is this workspace's");

    assert!(
        detail.ticks.is_empty(),
        "nothing has closed a task of this change: {:?}",
        detail.ticks
    );
}
