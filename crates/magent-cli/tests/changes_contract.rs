//! What `magent changes` has to answer.
//!
//! The three spec-driven skills open by running it in a `!` line, so its
//! output lands in front of the model before the turn starts. Two things
//! follow, and both are asserted here: it has to say something even when the
//! profile is empty — silence in that position reads as a broken install —
//! and what it says has to be legible to a reader rather than parsed.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use magent_core::{Classification, OperationId, PlanCommand, ProposeCommand, TaskDraft};
use magent_store::{FactContext, Store};

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
}

struct World {
    _dir: tempfile::TempDir,
    state_dir: PathBuf,
    project: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("state");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&state_dir).expect("mkdir");
        std::fs::create_dir_all(&project).expect("mkdir");

        git(&project, &["init", "-b", "main"]);
        git(&project, &["config", "user.email", "t@example.invalid"]);
        git(&project, &["config", "user.name", "T"]);

        Self {
            _dir: dir,
            state_dir,
            project,
        }
    }

    fn changes(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(MAGENT)
            .arg("changes")
            .args(args)
            .current_dir(&self.project)
            .env("MAGENT_STATE_DIR", &self.state_dir)
            .output()
            .expect("run magent changes");

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    fn database(&self) -> PathBuf {
        self.state_dir.join("magent.db")
    }

    /// Files a planned change under the workspace the command will resolve
    /// from the same directory, through the store rather than by writing rows:
    /// a fixture that bypasses `propose` and `plan` can hold a shape neither
    /// of them can produce.
    fn plan_a_change(&self, slug: &str, why: &str, tasks: &[(&str, &str)]) {
        let store = Store::open(&self.database()).expect("open the store");
        let context = FactContext {
            workspace_id: Some(
                store
                    .resolve_workspace_for(&self.project)
                    .expect("resolve the workspace")
                    .workspace_id,
            ),
            namespace: self
                .project
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            ..FactContext::default()
        };

        let change = store
            .propose(
                &ProposeCommand {
                    operation_id: OperationId::new(),
                    slug: slug.to_owned(),
                    title: "Add a retry budget".to_owned(),
                    classification: Classification::Bounded,
                    why: why.to_owned(),
                    what_changes: vec!["Give the worker a ceiling on retries".to_owned()],
                    capabilities: Vec::new(),
                    impact: None,
                    // No deltas, so the plan needs no requirement to cover:
                    // this fixture is about what the command prints, not about
                    // what the spec phase accepts.
                    skip_specs: true,
                },
                &context,
            )
            .expect("propose")
            .id;

        store
            .plan(
                &PlanCommand {
                    operation_id: OperationId::new(),
                    change,
                    tasks: tasks
                        .iter()
                        .map(|(number, title)| TaskDraft {
                            number: (*number).to_owned(),
                            title: (*title).to_owned(),
                            body: None,
                            files: Vec::new(),
                            consumes: None,
                            produces: None,
                            verify_command: "cargo test -p magent-cli".to_owned(),
                            expected_output: vec!["test result: ok".to_owned()],
                            covers: Vec::new(),
                        })
                        .collect(),
                },
                &context,
            )
            .expect("plan");
    }

    /// Closes one task the way executing the plan would leave it. The store
    /// has no verb for it yet, so the row is set directly, as
    /// `magent-store`'s own archive tests do.
    fn close_task(&self, number: &str) {
        let connection = rusqlite::Connection::open(self.database()).expect("open the store");
        let closed = connection
            .execute(
                "UPDATE tasks SET status = 'done' WHERE number = ?1",
                [number],
            )
            .expect("close the task");
        assert_eq!(closed, 1, "no task numbered {number} to close");
    }

    /// A tick in the journal, written directly rather than through a
    /// checkpoint: closing a task properly needs a run bound to the change and
    /// a plan whose command it can quote, and what this file tests is what the
    /// command prints, not how a tick gets there.
    fn journal_a_tick(&self, number: &str, command: &str, output: &str) {
        let connection = rusqlite::Connection::open(self.database()).expect("open the store");
        let change_id: String = connection
            .query_row("SELECT id FROM sdd_changes LIMIT 1", [], |row| row.get(0))
            .expect("the change");
        connection
            .execute(
                "INSERT INTO task_ticks
                     (id, change_id, number, verify_command, output, missing_json,
                      run_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?7)",
                rusqlite::params![
                    uuid_like(number),
                    change_id,
                    number,
                    command,
                    output,
                    uuid_like("run"),
                    "2026-08-18T12:00:00+00:00",
                ],
            )
            .expect("journal the tick");
    }
}

/// Any distinct value works where the store only stores an identifier.
fn uuid_like(seed: &str) -> String {
    format!("{seed:0>8}-0000-4000-8000-000000000000")
}

/// Silence here is indistinguishable from a broken install, and this runs
/// where a broken install is exactly what the reader would suspect.
#[test]
fn an_empty_profile_says_nothing_is_open() {
    let world = World::new();

    let (ok, report) = world.changes(&[]);

    assert!(ok, "an empty profile is not a failure: {report}");
    assert!(
        report.to_lowercase().contains("no change is open"),
        "an empty profile has to say so, not print nothing: {report}"
    );
}

#[test]
fn an_open_change_is_listed_with_its_task_counts() {
    let world = World::new();
    world.plan_a_change(
        "retry-budget",
        "Retries have no ceiling and can loop forever.",
        &[
            ("1", "Write the failing test"),
            ("2", "Add the budget"),
            ("3", "Wire it into the worker"),
        ],
    );
    world.close_task("1");

    let (ok, report) = world.changes(&[]);

    assert!(ok, "{report}");
    assert!(report.contains("retry-budget"), "{report}");
    assert!(
        report.contains("planned"),
        "the status is what says which step of the process is owed next: {report}"
    );
    assert!(
        report.contains("1/3"),
        "how much of the plan is closed is the whole reason to read this: {report}"
    );
}

#[test]
fn a_named_change_prints_its_tasks() {
    let world = World::new();
    world.plan_a_change(
        "retry-budget",
        "Retries have no ceiling and can loop forever.",
        &[
            ("1", "Write the failing test"),
            ("2", "Add the budget"),
            ("3", "Wire it into the worker"),
        ],
    );
    world.close_task("1");

    let (ok, report) = world.changes(&["retry-budget"]);

    assert!(ok, "{report}");
    assert!(
        report.contains("Retries have no ceiling and can loop forever."),
        "why the change is worth making is what a reader picking it up needs \
         first: {report}"
    );
    for title in [
        "Write the failing test",
        "Add the budget",
        "Wire it into the worker",
    ] {
        assert!(report.contains(title), "{title} missing from: {report}");
    }
    assert!(
        report.contains("done") && report.contains("pending"),
        "a task list that does not say which tasks are closed is a list of \
         work already done: {report}"
    );
}

/// A tick is worth no more than the evidence under it, and evidence nobody can
/// read is a checked box. The store keeps the journal precisely so a replan
/// cannot take it, and this command is where a person goes to look at it.
#[test]
fn a_named_change_prints_what_its_ticks_proved() {
    let world = World::new();
    world.plan_a_change(
        "retry-budget",
        "Retries have no ceiling and can loop forever.",
        &[("1", "Write the failing test"), ("2", "Add the budget")],
    );
    world.close_task("1");
    world.journal_a_tick(
        "1",
        "cargo test budget",
        "running 1 test\ntest caps_attempts ... ok\n",
    );

    let (ok, report) = world.changes(&["retry-budget"]);

    assert!(ok, "{report}");
    assert!(
        report.contains("cargo test budget"),
        "the command that proved the task is what makes the tick checkable: {report}"
    );
    assert!(
        report.contains("caps_attempts ... ok"),
        "and so is what it printed: {report}"
    );
}

/// A tick against a number the current plan no longer holds is the case the
/// journal exists for — a plan rewritten mid-execution. Printing it beside the
/// others without a word would read as a task that vanished from the list.
#[test]
fn a_tick_from_a_replaced_plan_is_marked_as_such() {
    let world = World::new();
    world.plan_a_change(
        "retry-budget",
        "Retries have no ceiling and can loop forever.",
        &[("1", "Write the failing test")],
    );
    world.journal_a_tick("9", "cargo test old", "test old_shape ... ok\n");

    let (ok, report) = world.changes(&["retry-budget"]);

    assert!(ok, "{report}");
    assert!(
        report.contains("cargo test old"),
        "a tick outlives its task, so it is still printed: {report}"
    );
    assert!(
        report.to_lowercase().contains("not in the current plan"),
        "a tick whose task the plan no longer holds has to say so: {report}"
    );
}
