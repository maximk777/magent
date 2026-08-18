//! The hook contract.
//!
//! Hooks are what make Magent trustworthy: they fire whether or not the model
//! cooperates. That cuts both ways — a hook that is slow, noisy or fatal
//! degrades every session, so the rules below are absolute:
//!
//! 1. never block, never fail the session;
//! 2. write nothing to stdout unless there is something worth injecting;
//! 3. stay inside the latency budget.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use magent_core::{HarnessKind, OperationId, SpecBinding, StartRunCommand};
use magent_store::Store;
use serde_json::{Value, json};

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

/// `PreCompact` runs on the critical path, between the decision to compact and
/// the compaction itself.
///
/// The release budget is 100 ms. Debug builds are several times slower and are
/// not what ships, so the assertion scales rather than being switched off:
/// a silent budget is a budget that rots.
fn precompact_budget() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_millis(400)
    } else {
        Duration::from_millis(100)
    }
}

struct HookRun {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    elapsed: Duration,
}

impl HookRun {
    fn succeeded(&self) -> bool {
        self.code == Some(0)
    }
}

fn run_hook(state_dir: &Path, event: &str, input: &Value) -> HookRun {
    run_hook_with_env(state_dir, event, input, &[])
}

fn run_hook_with_env(
    state_dir: &Path,
    event: &str,
    input: &Value,
    extra: &[(&str, &str)],
) -> HookRun {
    let started = Instant::now();
    let mut command = Command::new(MAGENT);
    for (key, value) in extra {
        command.env(key, value);
    }
    let mut child = command
        .args(["hook", event])
        .env("MAGENT_STATE_DIR", state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn magent");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.to_string().as_bytes())
        .expect("write hook input");

    let output = child.wait_with_output().expect("wait");
    HookRun {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
        elapsed: started.elapsed(),
    }
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(root: &Path) {
    std::fs::create_dir_all(root).expect("mkdir");
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.invalid"]);
    git(root, &["config", "user.name", "T"]);
    std::fs::write(root.join("README.md"), "seed\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "seed"]);
}

/// A state directory plus a repository to run sessions in.
struct Fixture {
    _dir: tempfile::TempDir,
    state_dir: std::path::PathBuf,
    repo: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("state");
        let repo = dir.path().join("project");
        std::fs::create_dir_all(&state_dir).expect("mkdir");
        init_repo(&repo);
        Self {
            _dir: dir,
            state_dir,
            repo,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.state_dir.join("magent.db")).expect("open store")
    }

    fn base(&self, event: &str, session: &str) -> Value {
        json!({
            "session_id": session,
            "transcript_path": self.state_dir.join("transcript.jsonl"),
            "cwd": self.repo,
            "hook_event_name": event,
        })
    }

    fn hook(&self, event: &str, input: &Value) -> HookRun {
        run_hook(&self.state_dir, event, input)
    }

    fn hook_with_env(&self, event: &str, input: &Value, extra: &[(&str, &str)]) -> HookRun {
        run_hook_with_env(&self.state_dir, event, input, extra)
    }
}

// --- degradation -----------------------------------------------------------

/// The single most important property. Magent being broken must cost the user
/// nothing: a failed hook that writes to stdout would inject garbage into the
/// conversation, and a non-zero exit would surface as a session error.
#[test]
fn a_corrupt_database_never_breaks_a_session() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.state_dir.join("magent.db"),
        b"this is definitely not a sqlite file",
    )
    .expect("corrupt the database");

    for event in [
        "session-start",
        "user-prompt-submit",
        "post-tool-use",
        "pre-compact",
        "session-end",
        "subagent-stop",
        "stop",
    ] {
        let run = fixture.hook(event, &fixture.base(event, "s1"));
        assert!(
            run.succeeded(),
            "{event} exited {:?}; hooks must never fail a session\n{}",
            run.code,
            run.stderr
        );
        assert!(
            run.stdout.is_empty(),
            "{event} wrote to stdout while degraded: {:?}",
            run.stdout
        );
    }
}

#[test]
fn malformed_hook_input_is_ignored_quietly() {
    let fixture = Fixture::new();

    let mut child = Command::new(MAGENT)
        .args(["hook", "session-start"])
        .env("MAGENT_STATE_DIR", &fixture.state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"{not json at all")
        .expect("write");
    let output = child.wait_with_output().expect("wait");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
}

// --- session start ---------------------------------------------------------

/// Nothing has happened yet, so there is nothing to say. Announcing itself here
/// would spend context on every single session for no benefit.
#[test]
fn session_start_is_silent_when_the_workspace_has_no_run() {
    let fixture = Fixture::new();

    let run = fixture.hook("session-start", &fixture.base("SessionStart", "s1"));

    assert!(run.succeeded());
    assert!(
        run.stdout.trim().is_empty(),
        "unexpected output: {}",
        run.stdout
    );
}

#[test]
fn session_start_restores_an_open_run_after_compaction() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let started = store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: "fix the payment timeout".into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![fixture.repo.clone()],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("start run");
    drop(store);

    let mut input = fixture.base("SessionStart", "s2");
    input["startup_reason"] = json!("compact");
    let run = fixture.hook("session-start", &input);

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("fix the payment timeout"),
        "the packet must carry the task:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&started.run_id.to_string()),
        "the packet must name the run so a later tool call can address it:\n{}",
        run.stdout
    );
}

/// The whole point of slice 1: the checkpoint written before compaction has to
/// come back afterwards.
#[test]
fn session_start_replays_the_latest_checkpoint() {
    let fixture = Fixture::new();
    let session = "s3";

    seed_run(&fixture, "trace the connection leak");
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("keep going"),
        ),
    );
    fixture.hook("pre-compact", &fixture.base("PreCompact", session));

    let mut restart = fixture.base("SessionStart", session);
    restart["startup_reason"] = json!("compact");
    let run = fixture.hook("session-start", &restart);

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("trace the connection leak"),
        "missing task:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.to_lowercase().contains("deterministic"),
        "the packet must say the checkpoint has not been enriched yet:\n{}",
        run.stdout
    );
}

/// The user's complaint that started this task: fifteen commits went straight
/// to `main` on an agreed spec change because nothing noticed. A run bound to
/// a spec change and left on `main` must say so, naming the branch.
#[test]
fn session_start_flags_a_spec_bound_run_left_on_main() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let started = store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: "wire the budget into the client".into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![fixture.repo.clone()],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("start run");
    store
        .bind_spec(
            started.run_id,
            &SpecBinding {
                change_id: Some("add-retry-budget".into()),
                current_task: None,
            },
        )
        .expect("bind spec");
    drop(store);

    let run = fixture.hook("session-start", &fixture.base("SessionStart", "s-main"));

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    // `main` alone would pass on the `Git:` line above whether or not the
    // note fired, so the branch is asserted as the note interpolates it.
    assert!(
        run.stdout.contains("agreed spec change") && run.stdout.contains("directly on `main`"),
        "the note must name the branch and the spec change in flight:\n{}",
        run.stdout
    );
}

/// The other half of the heuristic.
///
/// Review deleted `master` from the guard and every test stayed green: the
/// fixture always creates the repository with `-b main`, so half of what the
/// comment claims was covered by nothing.
#[test]
fn session_start_flags_a_spec_bound_run_left_on_master() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-b", "master"]);

    let store = fixture.store();
    let started = store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: "wire the budget into the client".into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![fixture.repo.clone()],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("start run");
    store
        .bind_spec(
            started.run_id,
            &SpecBinding {
                change_id: Some("add-retry-budget".into()),
                current_task: None,
            },
        )
        .expect("bind spec");
    drop(store);

    let run = fixture.hook("session-start", &fixture.base("SessionStart", "s-master"));

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("directly on `master`"),
        "the older default branch is half the heuristic:\n{}",
        run.stdout
    );
}

/// Same run, but on a branch of its own: the whole point is that Magent stays
/// quiet once a human has already made the call.
#[test]
fn session_start_says_nothing_on_a_feature_branch() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-b", "feature/x"]);
    let store = fixture.store();
    let started = store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: "wire the budget into the client".into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![fixture.repo.clone()],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("start run");
    store
        .bind_spec(
            started.run_id,
            &SpecBinding {
                change_id: Some("add-retry-budget".into()),
                current_task: None,
            },
        )
        .expect("bind spec");
    drop(store);

    let run = fixture.hook("session-start", &fixture.base("SessionStart", "s-feature"));

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        !run.stdout.contains("agreed spec change"),
        "a branch of its own is a decision already made, not one to flag:\n{}",
        run.stdout
    );
}

/// Ordinary work on `main` is the common case and must stay silent: the note
/// exists only for spec-driven work that landed on the default branch.
#[test]
fn session_start_says_nothing_without_a_spec_change() {
    let fixture = Fixture::new();
    seed_run(&fixture, "fix the flaky payment test");

    let run = fixture.hook("session-start", &fixture.base("SessionStart", "s-nospec"));

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        !run.stdout.contains("agreed spec change"),
        "no spec change is in flight, so there is nothing to flag:\n{}",
        run.stdout
    );
}

// --- run creation ----------------------------------------------------------

/// Hooks must not depend on the model calling `magent_start`. A prompt is the
/// start of a task whether or not the model chooses to announce it.
#[test]
fn a_first_prompt_opens_a_run_without_the_model_asking() {
    let fixture = Fixture::new();

    let input = with(
        fixture.base("UserPromptSubmit", "s4"),
        "prompt",
        json!("please fix the flaky payment test"),
    );
    let run = fixture.hook("user-prompt-submit", &input);
    assert!(run.succeeded(), "stderr: {}", run.stderr);

    let store = fixture.store();
    assert_eq!(store.run_count().expect("count"), 1);
}

#[test]
fn later_prompts_join_the_run_instead_of_opening_more() {
    let fixture = Fixture::new();

    for prompt in ["first thing", "second thing", "third thing"] {
        fixture.hook(
            "user-prompt-submit",
            &with(
                fixture.base("UserPromptSubmit", "s5"),
                "prompt",
                json!(prompt),
            ),
        );
    }

    assert_eq!(fixture.store().run_count().expect("count"), 1);
}

// --- toolchain detection ---------------------------------------------------

/// What a repository declares should be known before the model guesses at it.
/// Reading the manifests once, on first sight, costs a few file reads and
/// removes a whole class of confident wrong commands.
#[test]
fn first_sight_of_a_repository_reads_its_manifests() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.repo.join("go.mod"),
        "module github.com/acme/service\n\ngo 1.24.3\n",
    )
    .expect("write go.mod");

    // A run has to exist for session-start to attach to.
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", "t1"),
            "prompt",
            json!("start work"),
        ),
    );
    fixture.hook("session-start", &fixture.base("SessionStart", "t2"));

    let store = fixture.store();
    let found = store
        .search(&magent_store::FactQuery {
            text: Some("go module version".into()),
            namespaces: vec![
                fixture
                    .repo
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
            ],
            ..magent_store::FactQuery::default()
        })
        .expect("search");

    let go = found
        .iter()
        .find(|fact| fact.name == "toolchain-go")
        .expect("the go toolchain should have been detected");

    assert!(go.title.contains("1.24.3"), "{}", go.title);
    assert_eq!(
        go.status,
        magent_core::FactStatus::Observed,
        "reading a manifest is not the same as running anything"
    );
}

// --- the memory index ------------------------------------------------------

/// The index is what makes memory usable without the model knowing to ask. It
/// fires on every prompt, so it must be small, silent when it has nothing, and
/// free of bodies.
#[test]
fn a_prompt_pulls_a_relevant_memory_index_into_context() {
    let fixture = Fixture::new();
    let store = fixture.store();
    store
        .remember(
            &magent_core::RememberCommand {
                operation_id: OperationId::new(),
                name: "goose-table-locking".into(),
                title: "goose v3.26 locks with NewPostgresTableLocker".into(),
                body: "A LONG BODY THAT MUST NOT REACH THE CONTEXT WINDOW".into(),
                kind: magent_core::FactKind::Project,
                scope: magent_core::FactScope::Repository,
                cardinality: magent_core::Cardinality::Set,
                status: magent_core::FactStatus::Observed,
                confidence: 0.8,
                evidence: vec![],
                relates_to: vec![],
            },
            &magent_store::FactContext {
                namespace: Some(
                    fixture
                        .repo
                        .file_name()
                        .expect("name")
                        .to_string_lossy()
                        .into_owned(),
                ),
                ..magent_store::FactContext::default()
            },
        )
        .expect("remember");
    drop(store);

    let run = fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", "m1"),
            "prompt",
            json!("the goose migration hangs on a lock"),
        ),
    );

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("goose-table-locking"),
        "the index must name what exists:\n{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("LONG BODY"),
        "the index leaked a body into every prompt:\n{}",
        run.stdout
    );
}

/// Silence is the common case. A banner on every prompt with nothing behind it
/// is a tax with no return.
#[test]
fn a_prompt_that_matches_nothing_stays_silent() {
    let fixture = Fixture::new();

    let run = fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", "m2"),
            "prompt",
            json!("something entirely unrelated to anything remembered"),
        ),
    );

    assert!(run.succeeded());
    assert!(run.stdout.trim().is_empty(), "{}", run.stdout);
}

/// Re-pushing the same facts on every prompt of a session would make a long
/// session pay for the same tokens over and over.
#[test]
fn the_index_is_not_pushed_twice_in_one_session() {
    let fixture = Fixture::new();
    let store = fixture.store();
    store
        .remember(
            &magent_core::RememberCommand {
                operation_id: OperationId::new(),
                name: "no-autonomous-commits".into(),
                title: "never commit without being asked".into(),
                body: "the user makes the commits".into(),
                kind: magent_core::FactKind::Feedback,
                scope: magent_core::FactScope::User,
                cardinality: magent_core::Cardinality::Set,
                status: magent_core::FactStatus::Observed,
                confidence: 0.9,
                evidence: vec![],
                relates_to: vec![],
            },
            &magent_store::FactContext::default(),
        )
        .expect("remember");
    drop(store);

    let prompt = with(
        fixture.base("UserPromptSubmit", "m3"),
        "prompt",
        json!("should I commit this"),
    );

    let first = fixture.hook("user-prompt-submit", &prompt);
    assert!(
        first.stdout.contains("no-autonomous-commits"),
        "{}",
        first.stdout
    );

    let second = fixture.hook("user-prompt-submit", &prompt);
    assert!(
        !second.stdout.contains("no-autonomous-commits"),
        "the same fact was pushed twice:\n{}",
        second.stdout
    );
}

// --- the unrecorded-reasoning notice ----------------------------------------

/// Reasoning is the one thing hooks cannot capture, and asking the model to
/// judge when its own work became non-trivial is what failed on the real
/// profile: six of nine sessions that edited a file left no reasoning at all.
/// So the hook counts instead, and reports the count as a fact.
#[test]
fn a_run_with_work_and_no_reasoning_is_told_at_the_prompt() {
    let fixture = Fixture::new();
    let session = "n1";

    // The first prompt is what opens the run the edits are attributed to.
    assert!(
        fixture
            .hook(
                "user-prompt-submit",
                &prompt_of(&fixture, session, "rework the retry budget"),
            )
            .succeeded()
    );
    record_edits(&fixture, session, 12);

    let run = fixture.hook(
        "user-prompt-submit",
        &prompt_of(&fixture, session, "now wire it up"),
    );

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("12"),
        "the notice must report the count it counted:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("magent_checkpoint"),
        "the notice must name the tool that records reasoning:\n{}",
        run.stdout
    );
}

/// Below the threshold there is nothing to report. A notice on every short
/// session would be the same tax the memory index is careful not to charge.
#[test]
fn a_short_run_is_not_told() {
    let fixture = Fixture::new();
    let session = "n2";

    assert!(
        fixture
            .hook(
                "user-prompt-submit",
                &prompt_of(&fixture, session, "a small fix"),
            )
            .succeeded()
    );
    record_edits(&fixture, session, 3);

    let run = fixture.hook(
        "user-prompt-submit",
        &prompt_of(&fixture, session, "and one more"),
    );

    assert!(run.succeeded(), "stderr: {}", run.stderr);
    assert!(
        !run.stdout.contains("magent_checkpoint"),
        "three edits is not a session that has forgotten to explain itself:\n{}",
        run.stdout
    );
}

/// Repeating it every prompt would turn a fact into nagging, and nagging is
/// what gets tuned out.
#[test]
fn a_session_is_told_only_once() {
    let fixture = Fixture::new();
    let session = "n3";

    assert!(
        fixture
            .hook(
                "user-prompt-submit",
                &prompt_of(&fixture, session, "port the parser"),
            )
            .succeeded()
    );
    record_edits(&fixture, session, 12);

    let first = fixture.hook(
        "user-prompt-submit",
        &prompt_of(&fixture, session, "keep going"),
    );
    assert!(
        first.stdout.contains("magent_checkpoint"),
        "{}",
        first.stdout
    );

    let second = fixture.hook(
        "user-prompt-submit",
        &prompt_of(&fixture, session, "keep going"),
    );
    assert!(
        !second.stdout.contains("magent_checkpoint"),
        "the session was told twice:\n{}",
        second.stdout
    );
}

/// The notice is subject to the same rule as everything else in this file:
/// Magent being broken costs the user nothing.
#[test]
fn a_broken_store_still_answers_the_prompt() {
    let fixture = Fixture::new();
    let session = "n4";

    assert!(
        fixture
            .hook(
                "user-prompt-submit",
                &prompt_of(&fixture, session, "rework the retry budget"),
            )
            .succeeded()
    );
    record_edits(&fixture, session, 12);

    // The write-ahead log would otherwise carry the run past a corrupt header.
    for sidecar in ["magent.db-wal", "magent.db-shm"] {
        let _ = std::fs::remove_file(fixture.state_dir.join(sidecar));
    }
    std::fs::write(
        fixture.state_dir.join("magent.db"),
        b"this is definitely not a sqlite file",
    )
    .expect("corrupt the database");

    let run = fixture.hook(
        "user-prompt-submit",
        &prompt_of(&fixture, session, "now wire it up"),
    );

    assert!(
        run.succeeded(),
        "exited {:?}; hooks must never fail a session\n{}",
        run.code,
        run.stderr
    );
    assert!(
        run.stdout.is_empty(),
        "wrote to stdout while degraded: {:?}",
        run.stdout
    );
}

// --- the file ledger -------------------------------------------------------

#[test]
fn post_tool_use_records_edited_files() {
    let fixture = Fixture::new();
    seed_run(&fixture, "rework the client");
    let session = "s6";
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("go"),
        ),
    );

    let mut input = fixture.base("PostToolUse", session);
    input["tool_name"] = json!("Edit");
    input["tool_input"] = json!({ "file_path": fixture.repo.join("src/client.rs") });
    let run = fixture.hook("post-tool-use", &input);

    assert!(run.succeeded());
    assert!(
        run.stdout.is_empty(),
        "the ledger is silent; it exists to be read later, not narrated"
    );

    let store = fixture.store();
    let ledger = store
        .ledger_for_external_session(session, 50)
        .expect("ledger");
    assert!(
        ledger.iter().any(|entry| entry.path.ends_with("client.rs")),
        "expected client.rs in {ledger:?}"
    );
}

/// The ledger must store canonical paths.
///
/// Otherwise the restoration packet cannot shorten them: on macOS a temp or
/// symlinked directory reaches the ledger as `/var/...` while the repository
/// root resolves to `/private/var/...`, the prefixes fail to match, and every
/// file is printed in full on every session.
#[test]
fn restored_file_paths_are_relative_to_the_repository() {
    let fixture = Fixture::new();
    let session = "s12";
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("touch a few files"),
        ),
    );

    let edited = fixture.repo.join("crates/service/src/handler.rs");
    std::fs::create_dir_all(edited.parent().expect("parent")).expect("mkdir");
    std::fs::write(&edited, "fn main() {}\n").expect("write");

    let mut edit = fixture.base("PostToolUse", session);
    edit["tool_name"] = json!("Edit");
    edit["tool_input"] = json!({ "file_path": edited });
    fixture.hook("post-tool-use", &edit);

    let mut restart = fixture.base("SessionStart", "s13");
    restart["startup_reason"] = json!("compact");
    let run = fixture.hook("session-start", &restart);

    assert!(
        run.stdout.contains("crates/service/src/handler.rs"),
        "missing the file:\n{}",
        run.stdout
    );
    assert!(
        !run.stdout
            .contains(&fixture.repo.to_string_lossy().to_string()),
        "the repository prefix is repeated for every file:\n{}",
        run.stdout
    );
}

#[test]
fn post_tool_use_without_a_file_path_is_a_no_op() {
    let fixture = Fixture::new();
    let session = "s7";
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("go"),
        ),
    );

    let mut input = fixture.base("PostToolUse", session);
    input["tool_name"] = json!("Bash");
    input["tool_input"] = json!({ "command": "cargo test" });

    assert!(fixture.hook("post-tool-use", &input).succeeded());
}

// --- pre-compact -----------------------------------------------------------

#[test]
fn pre_compact_writes_a_deterministic_checkpoint_and_queues_enrichment() {
    let fixture = Fixture::new();
    let session = "s8";
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("make the retry budget configurable"),
        ),
    );

    let run = fixture.hook("pre-compact", &fixture.base("PreCompact", session));
    assert!(run.succeeded(), "stderr: {}", run.stderr);

    let store = fixture.store();
    let run_id = store
        .run_for_external_session(session)
        .expect("lookup")
        .expect("a run exists");
    assert_eq!(
        store.checkpoint_count(run_id).expect("count"),
        1,
        "compaction must never happen without a checkpoint behind it"
    );
    // Asked for, not claimed. `pre-compact` spawns a detached worker against
    // this same queue, so racing it to `claim_job` tests which of the two got
    // there first — not the contract, which is that compaction leaves the
    // enrichment queued for someone.
    assert!(
        store
            .job_state("enrich_checkpoint", &run_id.to_string())
            .expect("job state")
            .is_some(),
        "the reasoning behind the work is enriched asynchronously"
    );
}

/// Compaction is triggered by pressure. Blocking it, or making the user wait,
/// turns a routine event into a stall.
#[test]
fn pre_compact_stays_inside_its_latency_budget() {
    let fixture = Fixture::new();
    let session = "s9";
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("wide refactor"),
        ),
    );

    // A realistic ledger: a long session touches many files.
    for index in 0..200 {
        let mut input = fixture.base("PostToolUse", session);
        input["tool_name"] = json!("Edit");
        input["tool_input"] = json!({ "file_path": fixture.repo.join(format!("src/f{index}.rs")) });
        fixture.hook("post-tool-use", &input);
    }

    let run = fixture.hook("pre-compact", &fixture.base("PreCompact", session));

    assert!(run.succeeded());
    assert!(
        run.elapsed < precompact_budget(),
        "pre-compact took {:?}, budget is {:?}",
        run.elapsed,
        precompact_budget()
    );
}

/// `PreCompact` can only allow or deny; it cannot inject. Writing to stdout
/// here would be discarded at best and confusing at worst.
#[test]
fn pre_compact_never_blocks_compaction() {
    let fixture = Fixture::new();
    let run = fixture.hook("pre-compact", &fixture.base("PreCompact", "s10"));

    assert_eq!(
        run.code,
        Some(0),
        "exit 2 would block compaction and wedge the session"
    );
    assert!(run.stdout.is_empty());
}

// --- session end -----------------------------------------------------------

#[test]
fn session_end_closes_the_session_but_leaves_the_run_open() {
    let fixture = Fixture::new();
    let session = "s11";
    fixture.hook(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("something"),
        ),
    );

    assert!(
        fixture
            .hook("session-end", &fixture.base("SessionEnd", session))
            .succeeded()
    );

    let store = fixture.store();
    let run_id = store
        .run_for_external_session(session)
        .expect("lookup")
        .expect("run");
    assert_eq!(
        store.get_run(run_id).expect("get_run").status,
        magent_core::RunStatus::Open,
        "closing the editor does not finish the task"
    );

    let (queued, _) = store.job_counts().expect("job counts");
    assert_eq!(
        queued, 0,
        "nothing may be queued that no worker claims: a distill_session row \
         per session is a backlog that only grows"
    );
}

/// Inside the distiller's own `claude`, the hooks stand down.
///
/// Without this the summary of a run opens a run of its own, which compacts
/// and queues another distillation. The guard lives here rather than in a CLI
/// flag because this is a guarantee we can make ourselves.
#[test]
fn a_hook_running_inside_a_distillation_does_nothing() {
    let fixture = Fixture::new();
    let session = "s12";

    let run = fixture.hook_with_env(
        "user-prompt-submit",
        &with(
            fixture.base("UserPromptSubmit", session),
            "prompt",
            json!("this must not open a run"),
        ),
        &[(magent_distill::RECURSION_GUARD, "1")],
    );
    assert!(run.succeeded(), "stderr: {}", run.stderr);

    let store = fixture.store();
    assert!(
        store
            .run_for_external_session(session)
            .expect("lookup")
            .is_none(),
        "a distillation must not open a run of its own"
    );
}

// --- helpers ---------------------------------------------------------------

fn with(mut value: Value, key: &str, entry: Value) -> Value {
    value[key] = entry;
    value
}

fn record_edits(fixture: &Fixture, session: &str, count: usize) {
    for index in 0..count {
        let mut input = fixture.base("PostToolUse", session);
        input["tool_name"] = json!("Edit");
        input["tool_input"] = json!({ "file_path": fixture.repo.join(format!("src/e{index}.rs")) });
        assert!(fixture.hook("post-tool-use", &input).succeeded());
    }
}

fn prompt_of(fixture: &Fixture, session: &str, text: &str) -> Value {
    with(
        fixture.base("UserPromptSubmit", session),
        "prompt",
        json!(text),
    )
}

fn seed_run(fixture: &Fixture, task: &str) {
    let store = fixture.store();
    store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: task.into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![fixture.repo.clone()],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("seed run");
}
