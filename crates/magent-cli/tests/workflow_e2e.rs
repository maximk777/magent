//! The whole product, scenario by scenario.
//!
//! Every other test covers one seam. This covers the workflows a person
//! actually performs, driven only through the three surfaces they have — the
//! hook binary, the MCP server and the CLI — so a scenario passing here means
//! the parts are wired together, not merely correct in isolation.
//!
//! Nothing reaches into the store to arrange a state that the product cannot
//! reach on its own. Where a test reads the store, it is to check an outcome,
//! never to fake a precondition.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Map, Value, json};

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

// --- the world a person works in -------------------------------------------

/// A machine: one Magent profile, some repositories, and a corpus to import.
struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state_dir: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).expect("mkdir");

        Self {
            _dir: dir,
            root,
            state_dir,
        }
    }

    /// A git repository under `parent`, as a checkout on disk.
    fn repo(&self, parent: &str, name: &str, origin: Option<&str>) -> PathBuf {
        let path = self.root.join(parent).join(name);
        std::fs::create_dir_all(&path).expect("mkdir");

        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "T"],
        ] {
            git(&path, &args);
        }
        std::fs::write(path.join("README.md"), "seed\n").expect("write");
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "seed"]);

        if let Some(origin) = origin {
            git(&path, &["remote", "add", "origin", origin]);
        }

        path
    }

    fn store(&self) -> magent_store::Store {
        magent_store::Store::open(&self.state_dir.join("magent.db")).expect("open store")
    }

    /// Runs a `magent` subcommand, as a person would in a terminal.
    fn cli(&self, args: &[&str]) -> String {
        let output = Command::new(MAGENT)
            .args(args)
            .env("MAGENT_STATE_DIR", &self.state_dir)
            .output()
            .expect("run magent");

        assert!(
            output.status.success(),
            "magent {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// One harness session, working in `repo`.
    fn session(&self, id: &str, repo: &Path) -> Session {
        Session {
            state_dir: self.state_dir.clone(),
            repo: repo.to_path_buf(),
            id: id.to_owned(),
        }
    }
}

/// A Claude Code session: the events it fires and the tools it can call.
struct Session {
    state_dir: PathBuf,
    repo: PathBuf,
    id: String,
}

impl Session {
    fn hook(&self, event: &str, extra: &Value) -> String {
        let mut input = json!({
            "session_id": self.id,
            "cwd": self.repo,
            "transcript_path": self.state_dir.join("transcript.jsonl"),
        });
        if let Some(fields) = extra.as_object() {
            for (key, value) in fields {
                input[key] = value.clone();
            }
        }

        let mut child = Command::new(MAGENT)
            .args(["hook", event])
            .env("MAGENT_STATE_DIR", &self.state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hook");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.to_string().as_bytes())
            .expect("write");

        let output = child.wait_with_output().expect("wait");
        assert_eq!(
            output.status.code(),
            Some(0),
            "hook {event} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn start(&self, reason: &str) -> String {
        self.hook("session-start", &json!({ "startup_reason": reason }))
    }

    fn prompt(&self, text: &str) -> String {
        self.hook("user-prompt-submit", &json!({ "prompt": text }))
    }

    fn edit(&self, relative: &str, contents: &str) {
        let path = self.repo.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
        self.hook(
            "post-tool-use",
            &json!({ "tool_name": "Edit", "tool_input": { "file_path": path } }),
        );
    }

    fn compact(&self) -> String {
        self.hook("pre-compact", &json!({ "compaction_reason": "auto" }))
    }

    fn end(&self) -> String {
        self.hook("session-end", &json!({}))
    }

    /// Connects the MCP server for this working directory.
    async fn tools(&self) -> Client {
        let state_dir = self.state_dir.clone();
        let repo = self.repo.clone();

        let transport =
            TokioChildProcess::new(tokio::process::Command::new(MAGENT).configure(|command| {
                command
                    .arg("mcp")
                    .arg("--state-dir")
                    .arg(&state_dir)
                    .current_dir(&repo);
            }))
            .expect("spawn mcp");

        let service = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
            .await
            .expect("initialize in time")
            .expect("initialize");

        Client { service }
    }
}

struct Client {
    service: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
}

impl Client {
    async fn call(&self, tool: &'static str, arguments: Value) -> Value {
        let mut params = CallToolRequestParams::default();
        params.name = tool.into();
        params.arguments = Some(arguments.as_object().cloned().unwrap_or_else(Map::new));

        let result = tokio::time::timeout(Duration::from_secs(10), self.service.call_tool(params))
            .await
            .expect("call in time")
            .expect("call");

        assert!(
            result.is_error != Some(true),
            "{tool} failed: {:?}",
            result.content
        );

        let text = result
            .content
            .iter()
            .find_map(|item| item.as_text().map(|text| text.text.clone()))
            .unwrap_or_else(|| panic!("{tool} returned no text"));
        serde_json::from_str(&text).expect("json")
    }

    async fn close(self) {
        self.service.cancel().await.expect("shutdown");
    }
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
}

fn operation(tag: u32) -> String {
    format!("00000000-0000-4000-8000-{tag:012x}")
}

/// Writes a markdown corpus in the shape the importer reads.
fn write_corpus(root: &Path, entries: &[(&str, &str, &str, &str)]) {
    for (namespace, name, description, body) in entries {
        let path = root.join(namespace).join(format!("{name}.md"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            path,
            format!(
                "---\nname: {name}\ndescription: {description}\nmetadata:\n  type: project\n---\n\n{body}\n"
            ),
        )
        .expect("write");
    }
}

// ===========================================================================
// Scenario A — a day of work
// ===========================================================================

/// The complete arc: nothing in flight, a task begins, work happens, context is
/// compacted, the session ends, a new one resumes it the next morning, and the
/// task is finished deliberately.
#[tokio::test]
async fn a_task_survives_a_whole_day_of_interruptions() {
    let world = World::new();
    let repo = world.repo(
        "work",
        "service",
        Some("git@example.invalid:acme/service.git"),
    );
    std::fs::write(repo.join("go.mod"), "module acme/service\n\ngo 1.24.3\n").expect("write");

    // Morning: nothing is in flight, so nothing is said.
    let morning = world.session("morning", &repo);
    assert!(
        morning.start("startup").trim().is_empty(),
        "an empty workspace must not spend context announcing itself"
    );

    // A task begins with a request, not with the model announcing one.
    morning.prompt("the payment call times out under load");
    assert_eq!(world.store().run_count().expect("runs"), 1);

    // The model opens the run it is already in.
    let tools = morning.tools().await;
    let started = tools
        .call(
            "magent_start",
            json!({ "operation_id": operation(1), "task": "fix the payment timeout" }),
        )
        .await;
    let run_id = started["run_id"].as_str().expect("run_id").to_owned();
    assert_eq!(
        world.store().run_count().expect("runs"),
        1,
        "the model opened a second run for one task"
    );

    // Work happens; edits are recorded without being narrated.
    morning.edit("src/client.rs", "pub const RETRIES: u32 = 3;\n");

    // The model records what only it knows.
    tools
        .call(
            "magent_checkpoint",
            json!({
                "operation_id": operation(2),
                "run_id": run_id,
                "session_id": started["session_id"],
                "stage": "executing",
                "origin": "enriched",
                "completed_steps": ["found the hardcoded budget"],
                "next_steps": ["thread it through the config loader"],
                "decisions": ["make it configurable rather than raising the default"],
                "rejected": ["raising the default, which hides the latency"],
                "changed_files": ["src/client.rs"],
                "verification": ["the existing retry test still passes"],
                "risks": [],
                "handoff_summary": "budget located; config wiring is next"
            }),
        )
        .await;
    tools.close().await;

    // Midday: the context fills and is compacted.
    assert!(
        morning.compact().is_empty(),
        "pre-compact cannot inject, so it must stay silent"
    );
    let after_compaction = morning.start("compact");
    assert_context_carried(&after_compaction, &run_id);

    // Evening: the session ends. The task does not.
    morning.end();
    assert_eq!(
        world
            .store()
            .get_run(run_id.parse().expect("uuid"))
            .expect("run")
            .status,
        magent_core::RunStatus::Open,
        "closing an editor is not finishing a task"
    );

    // Next morning, a brand new session picks it up.
    let next_day = world.session("next-day", &repo);
    let resumed = next_day.start("startup");
    assert_context_carried(&resumed, &run_id);

    // And the work is finished deliberately.
    let tools = next_day.tools().await;
    let status = tools.call("magent_status", json!({})).await;
    let session_id = status["run"]["run_id"]
        .as_str()
        .map(|_| started["session_id"].clone())
        .expect("a run is in flight");

    tools
        .call(
            "magent_finish",
            json!({
                "operation_id": operation(3),
                "run_id": run_id,
                "session_id": session_id,
                "action": "complete_run",
                "outcome": "verified"
            }),
        )
        .await;
    tools.close().await;

    // A finished task stops announcing itself.
    let after = world.session("after", &repo);
    assert!(
        after.start("startup").trim().is_empty(),
        "a completed run kept being restored"
    );
}

fn assert_context_carried(packet: &str, run_id: &str) {
    assert!(packet.contains(run_id), "the run is not named:\n{packet}");
    assert!(
        packet.contains("fix the payment timeout"),
        "the task did not survive:\n{packet}"
    );
    assert!(
        packet.contains("config wiring is next"),
        "what to do next did not survive:\n{packet}"
    );
    assert!(
        packet.contains("raising the default"),
        "a rejected alternative did not survive, so it will be re-proposed:\n{packet}"
    );
}

// ===========================================================================
// Scenario B — memory, from an existing corpus to a new session
// ===========================================================================

/// Import a corpus, have it answer a prompt, learn something new, and get it all
/// back out again.
#[tokio::test]
async fn memory_is_imported_used_extended_and_exported() {
    let world = World::new();
    let repo = world.repo(
        "work",
        "service",
        Some("git@example.invalid:acme/service.git"),
    );

    let corpus = world.root.join("memory");
    write_corpus(
        &corpus,
        &[(
            "service",
            "goose-table-locking",
            "goose v3.26 locks with NewPostgresTableLocker",
            "goose_lock is auto-created and needs DDL rights.",
        )],
    );

    let report = world.cli(&["import", "--memory-dir", corpus.to_str().expect("utf-8")]);
    assert!(report.contains("1 fact"), "{report}");

    // A prompt about it pulls the name into context, without the body.
    let session = world.session("s1", &repo);
    session.prompt("hello");
    let index = session.prompt("the goose migration hangs on a lock");
    assert!(
        index.contains("goose-table-locking"),
        "the index did not name what memory holds:\n{index}"
    );
    assert!(
        !index.contains("DDL rights"),
        "the index leaked a body into every prompt:\n{index}"
    );

    // The same fact is not paid for twice in one session.
    let again = session.prompt("still stuck on that goose lock");
    assert!(
        !again.contains("goose-table-locking"),
        "the same fact was pushed twice:\n{again}"
    );

    // Following the name up returns the body.
    let tools = session.tools().await;
    let recalled = tools
        .call("magent_recall", json!({ "name": "goose-table-locking" }))
        .await;
    assert!(
        recalled["fact"]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("DDL rights"),
        "recall returned no body: {recalled}"
    );

    // Something new is learned.
    tools
        .call(
            "magent_remember",
            json!({
                "operation_id": operation(10),
                "name": "lock-timeout-raised",
                "title": "the goose lock timeout was raised to 90s",
                "body": "The probe budget was the real constraint.",
                "kind": "project",
                "scope": "repository",
                "cardinality": "single",
                "status": "observed",
                "confidence": 0.9
            }),
        )
        .await;
    tools.close().await;

    // A later session finds it.
    let tomorrow = world.session("s2", &repo);
    let tools = tomorrow.tools().await;
    let found = tools
        .call(
            "magent_search",
            json!({ "text": "lock timeout probe budget" }),
        )
        .await;
    assert!(
        found["facts"]
            .as_array()
            .expect("facts")
            .iter()
            .any(|fact| fact["name"] == "lock-timeout-raised"),
        "what was learned did not survive the session: {found}"
    );
    tools.close().await;

    // And everything can be taken back out as markdown.
    let exported = world.root.join("exported");
    let summary = world.cli(&["export", "--into", exported.to_str().expect("utf-8")]);
    assert!(summary.contains("2 fact"), "{summary}");
    assert!(
        exported.join("service/goose-table-locking.md").is_file(),
        "the imported fact did not come back out"
    );
    assert!(
        exported.join("service/lock-timeout-raised.md").is_file(),
        "what was learned here did not come back out"
    );
}

/// A single-valued fact replaces its predecessor without destroying it.
#[tokio::test]
async fn correcting_a_fact_keeps_what_was_believed_before() {
    let world = World::new();
    let repo = world.repo("work", "service", None);
    let session = world.session("s1", &repo);
    session.prompt("start");

    let tools = session.tools().await;
    for (tag, title) in [(20, "the retry budget is 3"), (21, "the retry budget is 5")] {
        tools
            .call(
                "magent_remember",
                json!({
                    "operation_id": operation(tag),
                    "name": "retry-budget",
                    "title": title,
                    "body": title,
                    "kind": "project",
                    "scope": "repository",
                    "cardinality": "single",
                    "status": "observed",
                    "confidence": 0.8
                }),
            )
            .await;
    }

    let current = tools
        .call("magent_recall", json!({ "name": "retry-budget" }))
        .await;
    assert_eq!(
        current["fact"]["title"].as_str(),
        Some("the retry budget is 5"),
        "the correction did not take"
    );
    tools.close().await;

    let history = world
        .store()
        .fact_history(
            "retry-budget",
            &magent_store::FactContext {
                namespace: Some("service".into()),
                ..magent_store::FactContext::default()
            },
        )
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "the earlier value was destroyed rather than superseded"
    );
}

// ===========================================================================
// Scenario C — several projects that belong together
// ===========================================================================

/// Memory has to reach across a group without flooding it.
#[tokio::test]
async fn a_workspace_shares_what_is_common_and_keeps_what_is_not() {
    let world = World::new();
    let clients = world.repo(
        "bank",
        "clients",
        Some("git@example.invalid:bank/clients.git"),
    );
    let accounts = world.repo(
        "bank",
        "accounts",
        Some("git@example.invalid:bank/accounts.git"),
    );

    // Memory about the group, filed under one project, as it always is.
    let corpus = world.root.join("memory");
    write_corpus(
        &corpus,
        &[
            (
                "platform",
                "service-auth",
                "HMAC for clients, Bearer for user-balance",
                "Every service in the group authenticates this way.",
            ),
            (
                "clients",
                "clients-quirk",
                "a quirk of the clients service only",
                "Nothing outside clients behaves like this.",
            ),
        ],
    );
    world.cli(&["import", "--memory-dir", corpus.to_str().expect("utf-8")]);

    world.cli(&[
        "workspace",
        "group",
        "--name",
        "bank",
        clients.to_str().expect("utf-8"),
        accounts.to_str().expect("utf-8"),
    ]);
    let promoted = world.cli(&[
        "workspace",
        "promote",
        "--namespace",
        "platform",
        "--into",
        "bank",
    ]);
    assert!(promoted.contains("promoted 1 fact"), "{promoted}");

    // Asked from the other service, the shared fact is there.
    let session = world.session("s1", &accounts);
    let tools = session.tools().await;
    let shared = tools
        .call(
            "magent_search",
            json!({ "text": "HMAC Bearer authenticate" }),
        )
        .await;
    assert!(
        shared["facts"]
            .as_array()
            .expect("facts")
            .iter()
            .any(|fact| fact["name"] == "service-auth"),
        "workspace memory did not reach a sibling: {shared}"
    );

    // The other service's own notes are not.
    let private = tools
        .call(
            "magent_search",
            json!({ "text": "quirk behaves like this" }),
        )
        .await;
    assert!(
        !private["facts"]
            .as_array()
            .expect("facts")
            .iter()
            .any(|fact| fact["name"] == "clients-quirk"),
        "one service's notes leaked to another: {private}"
    );
    tools.close().await;
}

/// Two checkouts of one repository are one project, and must not accumulate two
/// separate memories.
#[test]
fn a_second_checkout_of_one_repository_shares_its_memory() {
    let world = World::new();
    let origin = "git@example.invalid:acme/service.git";
    let first = world.repo("a", "service", Some(origin));
    let second = world.repo("b", "service-copy", Some(origin));

    world.session("s1", &first).prompt("working here");
    let restored = world.session("s2", &second).start("startup");

    assert!(
        restored.contains("working here"),
        "the second checkout did not see the first's run:\n{restored}"
    );
}

// ===========================================================================
// Scenario D — handing work to another agent
// ===========================================================================

/// The spec's central promise: a second agent continues the task without the
/// first one's transcript.
#[tokio::test]
async fn another_agent_continues_the_work_without_the_transcript() {
    let world = World::new();
    let repo = world.repo(
        "work",
        "service",
        Some("git@example.invalid:acme/service.git"),
    );

    let first = world.session("first-agent", &repo);
    first.prompt("trace the connection leak");
    let tools = first.tools().await;
    let started = tools
        .call(
            "magent_start",
            json!({ "operation_id": operation(30), "task": "trace the connection leak" }),
        )
        .await;
    let run_id = started["run_id"].as_str().expect("run_id").to_owned();

    tools
        .call(
            "magent_checkpoint",
            json!({
                "operation_id": operation(31),
                "run_id": run_id,
                "session_id": started["session_id"],
                "stage": "executing",
                "origin": "enriched",
                "completed_steps": ["found the pool is never closed on error"],
                "next_steps": ["add the close in the error path and a regression test"],
                "decisions": ["fix the leak rather than raising the pool size"],
                "rejected": ["raising the pool size, which delays the failure"],
                "changed_files": ["internal/pool.go"],
                "verification": ["reproduced under load"],
                "risks": ["the error path is shared with the retry logic"],
                "handoff_summary": "leak located in the error path; the fix and a test are next"
            }),
        )
        .await;
    tools.close().await;

    // A different agent, a different process, no transcript between them.
    let second = world.session("second-agent", &repo);
    let tools = second.tools().await;
    let resumed = tools
        .call(
            "magent_start",
            json!({
                "operation_id": operation(32),
                "task": "trace the connection leak",
                "resume_run_id": run_id
            }),
        )
        .await;

    assert_eq!(resumed["run_id"].as_str(), Some(run_id.as_str()));
    assert_ne!(
        resumed["session_id"], started["session_id"],
        "the handoff reused the first agent's session"
    );

    let checkpoint = &resumed["latest_checkpoint"];
    assert_eq!(
        checkpoint["handoff_summary"].as_str(),
        Some("leak located in the error path; the fix and a test are next")
    );
    assert!(
        checkpoint["rejected"]
            .as_array()
            .expect("rejected")
            .iter()
            .any(|item| item.as_str().unwrap_or_default().contains("pool size")),
        "the second agent will re-propose what the first already rejected"
    );
    assert!(
        checkpoint["risks"]
            .as_array()
            .expect("risks")
            .iter()
            .any(|item| item.as_str().unwrap_or_default().contains("retry logic")),
        "an open risk was not handed over"
    );
    tools.close().await;
}

// ===========================================================================
// Scenario E — the background worker
// ===========================================================================

/// Compaction queues the reasoning for later rather than blocking on it, and
/// draining the queue is safe even when the engine cannot run.
#[test]
fn compaction_queues_distillation_and_the_worker_survives_an_unusable_engine() {
    let world = World::new();
    let repo = world.repo("work", "service", None);

    let session = world.session("s1", &repo);
    session.prompt("make the retry budget configurable");
    session.edit("src/client.rs", "pub const RETRIES: u32 = 3;\n");
    session.compact();

    let store = world.store();
    let claimed = store
        .claim_job("enrich_checkpoint", Duration::from_mins(1))
        .expect("claim");
    assert!(
        claimed.is_some(),
        "compaction did not queue the reasoning for later"
    );
    drop(store);

    // The engine needs an authenticated CLI and a network, neither of which a
    // test has. Draining must still exit cleanly rather than take the session
    // down with it.
    let output = Command::new(MAGENT)
        .args(["distill", "--once"])
        .env("MAGENT_STATE_DIR", &world.state_dir)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("run the worker");

    assert_eq!(
        output.status.code(),
        Some(0),
        "the worker failed instead of reporting: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A way to leave the profile in a state a real machine reaches.
type Breakage = (&'static str, Box<dyn Fn(&World)>);

// ===========================================================================
// Scenario F — everything that can go wrong
// ===========================================================================

/// Magent being broken must cost the user nothing at all. Each of these is a
/// state a real machine reaches.
#[test]
fn every_failure_mode_leaves_the_session_working() {
    let world = World::new();
    let repo = world.repo("work", "service", None);

    let cases: Vec<Breakage> = vec![
        (
            "a corrupt database",
            Box::new(|world: &World| {
                std::fs::write(world.state_dir.join("magent.db"), b"not a database")
                    .expect("corrupt");
            }),
        ),
        (
            "a state directory that cannot be written",
            Box::new(|world: &World| {
                let _ = std::fs::remove_file(world.state_dir.join("magent.db"));
                let _ = std::fs::remove_dir_all(&world.state_dir);
                std::fs::write(&world.state_dir, b"a file where a directory should be")
                    .expect("block the directory");
            }),
        ),
    ];

    for (description, break_it) in cases {
        break_it(&world);
        let session = world.session("s1", &repo);

        for event in [
            "session-start",
            "user-prompt-submit",
            "post-tool-use",
            "pre-compact",
            "session-end",
            "subagent-stop",
            "stop",
        ] {
            let output = session.hook(event, &json!({ "prompt": "anything" }));
            assert!(
                output.is_empty(),
                "with {description}, {event} wrote to stdout: {output:?}"
            );
        }

        // Restore the directory for the next case.
        let _ = std::fs::remove_file(&world.state_dir);
        std::fs::create_dir_all(&world.state_dir).expect("mkdir");
    }
}

/// Directories outside git are ordinary places to work. Refusing them would
/// break sessions for no reason.
#[test]
fn work_outside_a_repository_is_still_remembered() {
    let world = World::new();
    let scratch = world.root.join("scratch");
    std::fs::create_dir_all(&scratch).expect("mkdir");

    let session = world.session("s1", &scratch);
    session.prompt("figure out the rate limit maths");
    session.compact();

    let restored = world.session("s2", &scratch).start("startup");
    assert!(
        restored.contains("rate limit maths"),
        "work outside git was not remembered:\n{restored}"
    );
}

/// An unknown event name must be ignored rather than treated as a failure: a
/// newer Claude Code may fire events this build has never heard of.
#[test]
fn an_unrecognised_event_is_ignored() {
    let world = World::new();
    let repo = world.repo("work", "service", None);
    let session = world.session("s1", &repo);

    let output = Command::new(MAGENT)
        .args(["hook", "some-future-event"])
        .env("MAGENT_STATE_DIR", &world.state_dir)
        .stdin(Stdio::null())
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    let _ = session;
}

/// Hooks fire concurrently in a real session — a tool call finishing while a
/// prompt is submitted. Nothing may be lost to contention.
#[test]
fn concurrent_hooks_do_not_lose_writes() {
    let world = World::new();
    let repo = world.repo("work", "service", None);
    let session = world.session("s1", &repo);
    session.prompt("wide refactor");

    let handles: Vec<_> = (0..8)
        .map(|index| {
            let state_dir = world.state_dir.clone();
            let repo = repo.clone();
            std::thread::spawn(move || {
                let path = repo.join(format!("src/f{index}.rs"));
                std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                std::fs::write(&path, "// touched\n").expect("write");

                let input = json!({
                    "session_id": "s1",
                    "cwd": repo,
                    "tool_name": "Edit",
                    "tool_input": { "file_path": path },
                });

                let mut child = Command::new(MAGENT)
                    .args(["hook", "post-tool-use"])
                    .env("MAGENT_STATE_DIR", &state_dir)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn");
                child
                    .stdin
                    .take()
                    .expect("stdin")
                    .write_all(input.to_string().as_bytes())
                    .expect("write");
                child.wait().expect("wait").success()
            })
        })
        .collect();

    for handle in handles {
        assert!(handle.join().expect("join"), "a concurrent hook failed");
    }

    let ledger = world
        .store()
        .ledger_for_external_session("s1", 100)
        .expect("ledger");
    assert_eq!(ledger.len(), 8, "a concurrent write was lost: {ledger:?}");
}
