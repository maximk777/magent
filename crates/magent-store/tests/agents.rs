//! Tests for recording the agents a session runs, and for reading back which
//! of them never came back.
//!
//! Subagents share their parent's session id by design, so nothing before this
//! writes an `agents` row. These tests exercise the two writers that do —
//! `Store::record_agent` and `Store::mark_agent_returned` — and the reader
//! built on top of them, `Store::agents_that_did_not_return`.

use chrono::{Duration, Utc};
use magent_core::{
    ChangeId, Classification, DeltaOp, HarnessKind, OperationId, PlanCommand, ProposeCommand,
    RequirementDraft, RunId, ScenarioDraft, SessionId, SpecBinding, SpecifyCommand,
    StartRunCommand, TaskDraft,
};
use magent_store::{FactContext, Store};
use rusqlite::OptionalExtension;

/// A store on a fresh temp path, plus a hint already bound to a run and
/// session — built the way `store_contract.rs`'s own fixtures build one, by
/// calling `bind_session` rather than writing rows directly.
struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Store,
    session_id: String,
    root: std::path::PathBuf,
    context: FactContext,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("magent.db");
        let store = Store::open(&path).expect("open");

        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir");

        let bound = store
            .bind_session(
                "agent-fixture-hint",
                &root,
                "run subagents",
                HarnessKind::ClaudeCode,
            )
            .expect("bind session");

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
            session_id: bound.session_id.to_string(),
            root,
            context,
        }
    }

    const HINT: &'static str = "agent-fixture-hint";

    /// Change slug carrying the single task `run_holding` claims. Fixed
    /// rather than per-test, because each test opens its own store and the
    /// slug never has to disambiguate against another change in it.
    const SLUG: &'static str = "name-the-agents-at-large";
    const CAPABILITY: &'static str = "worker/hold";
    const TASK_NUMBER: &'static str = "1";
    /// A second task of the same plan, for the test that needs two sessions
    /// of one run each holding a different task.
    const TASK_NUMBER_TWO: &'static str = "2";
    const VERIFY: &'static str = "cargo test -p worker hold";
    const EXPECTED: &'static str = "test result: ok. 1 passed";
    /// Long enough to clear `magent-core`'s 50-character floor on a purpose.
    const PURPOSE: &'static str = "Recording which agents of a run were seen and never came \
                                    back, so a reader can act on the type rather than a bare id.";

    fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).expect("raw connection")
    }

    /// A run of its own, bound to no change and holding no task — a fresh
    /// hint on the same workspace, which `start_run` always turns into a new
    /// run rather than joining the one `HINT` already opened.
    fn simple_run(&self, hint: &str) -> RunId {
        self.store
            .start_run(
                &StartRunCommand {
                    operation_id: OperationId::new(),
                    task: "run a subagent".into(),
                    resume_run_id: None,
                    external_session_hint: Some(hint.into()),
                    workspace_roots: vec![self.root.clone()],
                },
                HarnessKind::ClaudeCode,
            )
            .expect("start")
            .run_id
    }

    /// A change carried to `planned`, with two tasks — `"1"` and `"2"` — so
    /// two sessions of one run can each hold a different one.
    fn planned_change(&self) -> ChangeId {
        let change = self
            .store
            .propose(
                &ProposeCommand {
                    operation_id: OperationId::new(),
                    slug: Self::SLUG.into(),
                    title: "Name the agents at large".into(),
                    classification: Classification::Bounded,
                    why: "Nothing today says which agent under a lapsed hold never came back."
                        .into(),
                    what_changes: vec!["Add a query naming unreturned agents".into()],
                    capabilities: vec![Self::CAPABILITY.into()],
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
                    capability_path: Self::CAPABILITY.into(),
                    purpose: Some(Self::PURPOSE.into()),
                    requirements: vec![RequirementDraft {
                        op: DeltaOp::Added,
                        name: "The worker holds a task under a live lease".into(),
                        text: Some("The worker SHALL hold a task under a live lease.".into()),
                        rename_to: None,
                        reason: None,
                        migration: None,
                        scenarios: vec![ScenarioDraft {
                            name: "task held".into(),
                            given: None,
                            when: "a task is claimed".into(),
                            then: "the lease is live".into(),
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
                    tasks: vec![
                        TaskDraft {
                            number: Self::TASK_NUMBER.into(),
                            title: "Hold the task".into(),
                            body: Some("Claim the task under a lease.".into()),
                            files: vec!["crates/worker/src/hold.rs".into()],
                            consumes: Vec::new(),
                            produces: vec!["fn hold(&mut self)".into()],
                            verify_command: Self::VERIFY.into(),
                            expected_output: vec![Self::EXPECTED.into()],
                            covers: vec!["The worker holds a task under a live lease".into()],
                        },
                        TaskDraft {
                            number: Self::TASK_NUMBER_TWO.into(),
                            title: "Hold a second task".into(),
                            body: Some(
                                "Claim a second, independent task under its own lease.".into(),
                            ),
                            files: vec!["crates/worker/src/hold_two.rs".into()],
                            consumes: Vec::new(),
                            produces: vec!["fn hold_two(&mut self)".into()],
                            verify_command: Self::VERIFY.into(),
                            expected_output: vec![Self::EXPECTED.into()],
                            covers: vec!["The worker holds a task under a live lease".into()],
                        },
                    ],
                    check_only: false,
                },
                &self.context,
            )
            .expect("plan");

        change
    }

    /// A run bound to `planned_change`'s change, holding task `"1"` — the
    /// way production takes a hold, through `bind_spec` naming the task
    /// rather than by writing `claimed_by` directly.
    fn run_holding(&self, hint: &str) -> RunId {
        let run_id = self.simple_run(hint);
        self.claim(run_id, Self::TASK_NUMBER);
        run_id
    }

    /// Claims `task_number` of `planned_change`'s change on `run_id` — the
    /// way production takes a hold, through `bind_spec` naming the task
    /// rather than by writing `claimed_by` directly. `write_binding`'s claim
    /// (`store.rs`) hands the hold to whichever session of `run_id` was seen
    /// most recently, so a caller with more than one session on the run must
    /// arrange that ordering itself — see `join_run`.
    fn claim(&self, run_id: RunId, task_number: &str) {
        self.store
            .bind_spec(
                run_id,
                &SpecBinding {
                    change_id: Some(Self::SLUG.into()),
                    current_task: Some(format!("{task_number}: hold the task")),
                },
            )
            .expect("bind");
    }

    /// A second session joined to `run_id` by resuming it explicitly, rather
    /// than by relying on `bind_session`'s "latest open run of this
    /// workspace", which by the time a test needs a second session may no
    /// longer name this one.
    ///
    /// Its `last_seen_at` is pushed to now on the way out, so the claim a
    /// test makes right after this call lands on the session just created
    /// rather than on whichever session held the previous task — the same
    /// ordering `write_binding`'s subquery relies on in production, driven
    /// here instead of by a hook's real traffic.
    fn join_run(&self, run_id: RunId, hint: &str) -> SessionId {
        let session_id = self
            .store
            .start_run(
                &StartRunCommand {
                    operation_id: OperationId::new(),
                    task: "join the run".into(),
                    resume_run_id: Some(run_id),
                    external_session_hint: Some(hint.into()),
                    workspace_roots: vec![self.root.clone()],
                },
                HarnessKind::ClaudeCode,
            )
            .expect("join")
            .session_id;

        self.store
            .touch_external_session(hint)
            .expect("stamp the joined session as the most recently seen");

        session_id
    }

    /// The session currently claiming `task_number` of `change`, read back
    /// to confirm a claim actually landed on the session a test meant it
    /// for, before the test trusts anything built on that assumption.
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

    /// Lapses the hold `planned_change`'s `task_number` task carries, by
    /// pushing `lease_until` an hour into the past. Scoped by change as well
    /// as by number: `tasks_number` is `UNIQUE(change_id, number)`, not
    /// unique on the number alone.
    fn lapse_hold(&self, change: ChangeId, task_number: &str) {
        self.raw()
            .execute(
                "UPDATE tasks SET lease_until = ?1 WHERE number = ?2 AND change_id = ?3",
                rusqlite::params![
                    (Utc::now() - Duration::hours(1)).to_rfc3339(),
                    task_number,
                    change.to_string(),
                ],
            )
            .expect("lapse the hold");
    }

    /// Reads back `(session_id, agent_type, started_at, ended_at)` for
    /// `agent_id`, if a row exists.
    fn agent_row(
        &self,
        agent_id: &str,
    ) -> Option<(String, Option<String>, String, Option<String>)> {
        self.raw()
            .query_row(
                "SELECT session_id, agent_type, started_at, ended_at FROM agents WHERE id = ?1",
                [agent_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .expect("query agents")
    }

    fn agent_count(&self) -> i64 {
        self.raw()
            .query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))
            .expect("count agents")
    }
}

#[test]
fn an_agent_is_recorded_once_however_often_it_is_seen() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-1", Some("reviewer"))
        .expect("first sighting");
    let first_seen = fixture
        .agent_row("agent-1")
        .expect("recorded on first sighting")
        .2;

    // A different type on the later sightings: if INSERT OR IGNORE were not
    // protecting the row, this would overwrite agent_type along with
    // started_at, and the assertion below would not catch a swapped bind
    // order or a first-write-loses regression.
    fixture
        .store
        .record_agent(Fixture::HINT, "agent-1", Some("implementer"))
        .expect("second sighting");
    fixture
        .store
        .record_agent(Fixture::HINT, "agent-1", Some("implementer"))
        .expect("third sighting");

    assert_eq!(
        fixture.agent_count(),
        1,
        "the same agent id must not produce more than one row however often it is seen"
    );
    let (_, agent_type, third_seen, _) = fixture.agent_row("agent-1").expect("still recorded");
    assert_eq!(
        first_seen, third_seen,
        "started_at must not move once the agent's first row exists"
    );
    assert_eq!(
        agent_type.as_deref(),
        Some("reviewer"),
        "the first sighting's agent_type must survive later sightings that pass a different type"
    );
}

#[test]
fn an_agent_names_the_session_it_ran_under() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-2", Some("reviewer"))
        .expect("record");

    let (session_id, agent_type, _, _) = fixture
        .agent_row("agent-2")
        .expect("the agent must be recorded");
    assert_eq!(
        session_id, fixture.session_id,
        "the recorded row must name the session its hint resolves to"
    );
    assert_eq!(
        agent_type.as_deref(),
        Some("reviewer"),
        "the recorded row must store the agent_type it was given"
    );
}

#[test]
fn an_agent_recorded_with_no_type_reads_back_as_none() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-untyped", None)
        .expect("record");

    let (_, agent_type, _, _) = fixture
        .agent_row("agent-untyped")
        .expect("the agent must be recorded");
    assert_eq!(
        agent_type, None,
        "a missing agent_type must stay NULL rather than becoming an empty string"
    );
}

#[test]
fn an_agent_of_an_unknown_session_is_not_recorded() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent("no-such-hint", "agent-3", Some("worker"))
        .expect("must not error when the hint carries no session");

    assert_eq!(
        fixture.agent_count(),
        0,
        "an agent whose parent has no session row must not be recorded"
    );
}

#[test]
fn a_blank_agent_id_records_nothing() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "", Some("worker"))
        .expect("must not error on an empty id");
    fixture
        .store
        .record_agent(Fixture::HINT, "   ", Some("worker"))
        .expect("must not error on a whitespace-only id");

    assert_eq!(
        fixture.agent_count(),
        0,
        "a blank agent id must never produce a row, since a legal empty TEXT PRIMARY KEY would merge unrelated agents"
    );
}

#[test]
fn a_blank_agent_id_marks_nothing_returned() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-real", Some("reviewer"))
        .expect("record");

    fixture
        .store
        .mark_agent_returned("")
        .expect("must not error on an empty id");
    fixture
        .store
        .mark_agent_returned("   ")
        .expect("must not error on a whitespace-only id");

    let ended = fixture.agent_row("agent-real").expect("recorded").3;
    assert_eq!(
        ended, None,
        "a blank id must not mark an unrelated real agent returned"
    );
}

#[test]
fn an_agent_is_marked_returned_once() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-4", Some("reviewer"))
        .expect("record");
    fixture
        .store
        .mark_agent_returned("agent-4")
        .expect("first return");

    let ended_first = fixture
        .agent_row("agent-4")
        .expect("recorded")
        .3
        .expect("ended_at must be set after the agent returns");

    fixture
        .store
        .mark_agent_returned("agent-4")
        .expect("second return");
    let ended_second = fixture
        .agent_row("agent-4")
        .expect("recorded")
        .3
        .expect("ended_at must still be set");

    assert_eq!(
        ended_first, ended_second,
        "a repeated SubagentStop must not move ended_at once it is set"
    );
}

#[test]
fn an_agent_that_never_returned_has_no_end() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-5", Some("reviewer"))
        .expect("record");

    let ended = fixture.agent_row("agent-5").expect("recorded").3;
    assert_eq!(
        ended, None,
        "an agent with no SubagentStop event must have no ended_at"
    );
}

#[test]
fn an_agent_of_another_run_is_not_named() {
    let fixture = Fixture::new();

    let run_a = fixture.simple_run("hint-run-a");
    let run_b = fixture.simple_run("hint-run-b");

    fixture
        .store
        .record_agent("hint-run-a", "agent-a", Some("reviewer"))
        .expect("record on run a");
    fixture
        .store
        .record_agent("hint-run-b", "agent-b", Some("reviewer"))
        .expect("record on run b");

    let named_on_a = fixture
        .store
        .agents_that_did_not_return(run_a)
        .expect("query run a");
    let named_on_b = fixture
        .store
        .agents_that_did_not_return(run_b)
        .expect("query run b");

    assert_eq!(
        named_on_a
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        vec!["agent-a"],
        "only the asked run's unreturned agent must be named, never the other run's"
    );
    assert_eq!(
        named_on_b
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        vec!["agent-b"],
        "the scope must hold symmetrically for the other run too"
    );
}

#[test]
fn an_agent_with_no_return_and_a_lapsed_lease_is_named() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let run_id = fixture.run_holding("hint-lapsed");

    fixture
        .store
        .record_agent("hint-lapsed", "agent-lapsed", Some("reviewer"))
        .expect("record");

    fixture.lapse_hold(change, Fixture::TASK_NUMBER);

    let named = fixture
        .store
        .agents_that_did_not_return(run_id)
        .expect("query");

    assert_eq!(
        named
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        vec!["agent-lapsed"],
        "an agent recorded with no return, whose session's hold has lapsed, must be named"
    );
}

#[test]
fn an_agent_that_returned_is_not_named() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let run_id = fixture.run_holding("hint-returned");

    fixture
        .store
        .record_agent("hint-returned", "agent-returned", Some("reviewer"))
        .expect("record");

    fixture.lapse_hold(change, Fixture::TASK_NUMBER);

    fixture
        .store
        .mark_agent_returned("agent-returned")
        .expect("mark returned");

    let named = fixture
        .store
        .agents_that_did_not_return(run_id)
        .expect("query");

    assert!(
        named.is_empty(),
        "an agent that returned must not be named even though its session's hold lapsed"
    );
}

#[test]
fn an_agent_whose_session_still_holds_a_task_is_not_named() {
    let fixture = Fixture::new();
    fixture.planned_change();
    let run_id = fixture.run_holding("hint-live");

    fixture
        .store
        .record_agent("hint-live", "agent-live", Some("reviewer"))
        .expect("record");

    let named = fixture
        .store
        .agents_that_did_not_return(run_id)
        .expect("query");

    assert!(
        named.is_empty(),
        "a live lease means somebody is still reporting, so the agent must not be named"
    );
}

#[test]
fn agents_that_did_not_return_names_the_recorded_agent_type() {
    let fixture = Fixture::new();
    let run_id = fixture.simple_run("hint-typed");

    fixture
        .store
        .record_agent("hint-typed", "agent-typed", Some("spec-reviewer"))
        .expect("record");

    let named = fixture
        .store
        .agents_that_did_not_return(run_id)
        .expect("query");

    assert_eq!(named.len(), 1, "the unreturned agent must be named");
    assert_eq!(
        named[0].agent_type.as_deref(),
        Some("spec-reviewer"),
        "the agent_type in the result must be the one record_agent recorded, not None"
    );
}

/// The query correlates `NOT EXISTS` on `a.session_id` — the candidate
/// agent's own session — never on `s.run_id`. Every other test in this file
/// puts one session on a run, so a query rewritten to exclude an agent
/// whenever *anybody on the run* still holds would pass all of them too.
/// This is the one that would catch that rewrite: two sessions share one
/// run, session one's hold stays live while session two's lapses, and the
/// agent recorded under session two must still be named — a live lease
/// belonging to somebody else's session must not silence an agent whose own
/// session has gone quiet.
#[test]
fn an_agent_is_named_though_another_session_of_the_run_still_holds() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();

    let run_id = fixture.simple_run("hint-quiet-session-one");
    fixture.claim(run_id, Fixture::TASK_NUMBER);

    let session_two = fixture.join_run(run_id, "hint-quiet-session-two");
    fixture.claim(run_id, Fixture::TASK_NUMBER_TWO);

    // The claims must have landed where this test relies on them landing,
    // before anything built on that assumption is trusted.
    assert_eq!(
        fixture.holder_of(change, Fixture::TASK_NUMBER_TWO),
        Some(session_two),
        "the second claim must land on the session just joined, not on session one"
    );
    assert_ne!(
        fixture.holder_of(change, Fixture::TASK_NUMBER),
        fixture.holder_of(change, Fixture::TASK_NUMBER_TWO),
        "the two tasks must be held by two different sessions"
    );

    // Session one's hold on task "1" is left live. Only session two's hold
    // on task "2" lapses.
    fixture.lapse_hold(change, Fixture::TASK_NUMBER_TWO);

    fixture
        .store
        .record_agent(
            "hint-quiet-session-two",
            "agent-quiet-two",
            Some("reviewer"),
        )
        .expect("record");

    let named = fixture
        .store
        .agents_that_did_not_return(run_id)
        .expect("query");

    assert_eq!(
        named
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        vec!["agent-quiet-two"],
        "session one's live lease belongs to a different session and must not silence an \
         agent whose own session's hold has lapsed"
    );
}

/// `ORDER BY a.started_at` alone has no tie-break, so two agents recorded in
/// the same instant come back in whatever order `SQLite` happens to choose —
/// and this is a report a person reads to decide whose work to chase.
/// Pinning both to one instant removes `started_at` as a tie-breaker on its
/// own, so only the `a.id` tie-break can be deciding the order asserted
/// below; recording them in the reverse of that order rules out an
/// accidental match against insertion order too.
#[test]
fn agents_named_together_keep_a_stable_order_across_reads() {
    let fixture = Fixture::new();
    let change = fixture.planned_change();
    let run_id = fixture.run_holding("hint-two-agents");

    fixture
        .store
        .record_agent("hint-two-agents", "zeta-agent", Some("reviewer"))
        .expect("record zeta");
    fixture
        .store
        .record_agent("hint-two-agents", "alpha-agent", Some("reviewer"))
        .expect("record alpha");

    let same_instant = Utc::now().to_rfc3339();
    fixture
        .raw()
        .execute(
            "UPDATE agents SET started_at = ?1 WHERE id = ?2 OR id = ?3",
            rusqlite::params![same_instant, "zeta-agent", "alpha-agent"],
        )
        .expect("pin both agents to the same instant");

    fixture.lapse_hold(change, Fixture::TASK_NUMBER);

    let named = fixture
        .store
        .agents_that_did_not_return(run_id)
        .expect("query");

    assert_eq!(
        named
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha-agent", "zeta-agent"],
        "two agents recorded at the same instant, and named in this run's next report, must \
         come back in the same order every time — a report naming several agents must not \
         shuffle between reads"
    );
}
