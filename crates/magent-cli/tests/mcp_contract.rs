//! The MCP contract.
//!
//! Exercised through the official RMCP client over stdio rather than by calling
//! handlers directly: what matters is the protocol a harness actually sees.
//!
//! Two limits shape this surface and are asserted here rather than trusted:
//! Claude Code truncates server instructions at 2 KB, and anything a server
//! writes to stdout that is not a protocol frame corrupts the session.

use std::{path::Path, time::Duration};

use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Map, Value, json};
use tokio::process::Command;

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

/// Claude Code truncates server instructions at this size, critical detail
/// first. Exceeding it silently loses the end of the bootstrap contract.
const INSTRUCTIONS_LIMIT: usize = 2048;

/// Slice 1 exposes exactly these. The old "only four tools" constraint was
/// about context cost, and tool search removed it — only names and server
/// instructions load at session start — but the set still grows deliberately.
const EXPECTED_TOOLS: [&str; 7] = [
    "magent_checkpoint",
    "magent_finish",
    "magent_recall",
    "magent_remember",
    "magent_search",
    "magent_start",
    "magent_status",
];

/// Tools that change state. Each must take an `operation_id` so a retry after a
/// dropped connection cannot duplicate work.
const MUTATING_TOOLS: [&str; 4] = [
    "magent_checkpoint",
    "magent_finish",
    "magent_remember",
    "magent_start",
];

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
}

fn init_repo(root: &Path) {
    std::fs::create_dir_all(root).expect("mkdir");
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "t@example.invalid"],
        vec!["config", "user.name", "T"],
    ] {
        let output = std::process::Command::new("git")
            .args(&args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}");
    }
    std::fs::write(root.join("README.md"), "seed\n").expect("write");
    for args in [vec!["add", "."], vec!["commit", "-m", "seed"]] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
    }
}

type Client = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

async fn connect(fixture: &Fixture) -> Client {
    let state_dir = fixture.state_dir.clone();
    let repo = fixture.repo.clone();

    let transport = TokioChildProcess::new(Command::new(MAGENT).configure(|command| {
        command
            .arg("mcp")
            .arg("--state-dir")
            .arg(&state_dir)
            .current_dir(&repo);
    }))
    .expect("spawn magent mcp");

    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .expect("initialize did not time out")
        .expect("initialize")
}

/// Calls a tool and parses its JSON payload, failing loudly on a tool error.
async fn call(client: &Client, tool: &'static str, arguments: Value) -> Value {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.call_tool(request(tool, &arguments)),
    )
    .await
    .expect("tool call did not time out")
    .expect("tool call");

    assert!(
        result.is_error != Some(true),
        "{tool} returned an error: {:?}",
        result.content
    );

    let text = first_text(&result).unwrap_or_else(|| panic!("{tool} returned no text content"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{tool} returned non-JSON: {error}"))
}

async fn call_expecting_error(client: &Client, tool: &'static str, arguments: Value) -> String {
    let result = client
        .call_tool(request(tool, &arguments))
        .await
        .expect("tool call");

    assert_eq!(result.is_error, Some(true), "expected {tool} to fail");
    first_text(&result).unwrap_or_default()
}

/// `CallToolRequestParams` is `#[non_exhaustive]`, so it is built from its
/// default rather than constructed literally.
fn request(tool: &'static str, arguments: &Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::default();
    params.name = tool.into();
    params.arguments = Some(arguments.as_object().cloned().unwrap_or_else(Map::new));
    params
}

fn first_text(result: &rmcp::model::CallToolResult) -> Option<String> {
    result
        .content
        .iter()
        .find_map(|item| item.as_text().map(|text| text.text.clone()))
}

// --- surface ---------------------------------------------------------------

#[tokio::test]
async fn the_server_exposes_exactly_the_slice_one_tools() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .expect("list tools")
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();

    assert_eq!(names, EXPECTED_TOOLS);
    client.cancel().await.expect("shutdown");
}

/// Server instructions are the bootstrap contract: they are what tells the
/// model when to open a run and when to checkpoint. Losing the tail to
/// truncation would silently drop part of that.
#[tokio::test]
async fn server_instructions_fit_the_two_kilobyte_limit() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("the server must ship instructions");

    assert!(!instructions.trim().is_empty());
    assert!(
        instructions.len() <= INSTRUCTIONS_LIMIT,
        "instructions are {} bytes, limit is {INSTRUCTIONS_LIMIT}",
        instructions.len()
    );
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn every_mutating_tool_requires_an_operation_id() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    for tool in client.list_all_tools().await.expect("list tools") {
        if !MUTATING_TOOLS.contains(&tool.name.as_ref()) {
            continue;
        }

        let required = tool
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{} has no required fields", tool.name));

        assert!(
            required.iter().any(|field| field == "operation_id"),
            "{} does not require operation_id: {required:?}",
            tool.name
        );
    }

    client.cancel().await.expect("shutdown");
}

/// The harness never tells the server which harness it is; the server knows
/// from how it was launched. A client-supplied field would let a session
/// misreport itself.
#[tokio::test]
async fn start_does_not_accept_a_client_supplied_harness() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let start = client
        .list_all_tools()
        .await
        .expect("list tools")
        .into_iter()
        .find(|tool| tool.name == "magent_start")
        .expect("magent_start");

    let properties = start
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("properties");

    assert!(
        !properties.contains_key("harness"),
        "magent_start exposes a harness field: {:?}",
        properties.keys().collect::<Vec<_>>()
    );
    client.cancel().await.expect("shutdown");
}

// --- behaviour -------------------------------------------------------------

/// The checkpoint is the whole point of the server, and it was the hardest
/// tool to call: the schema demanded a session id and an origin the model has
/// no way to know, and every list field. Seven attempts to record one
/// checkpoint is a tool that will be skipped instead.
#[tokio::test]
async fn a_checkpoint_needs_only_a_stage_and_a_summary() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "fix the payment timeout" }),
    )
    .await;

    let saved = call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "owner traced; regression test is next"
        }),
    )
    .await;

    assert!(saved["checkpoint_id"].is_string(), "{saved}");

    let status = call(&client, "magent_status", json!({})).await;
    assert_eq!(
        status["run"]["latest_checkpoint"]["handoff_summary"].as_str(),
        Some("owner traced; regression test is next"),
        "the checkpoint landed on the open run without being told which"
    );
    // A checkpoint the model wrote is enriched by definition; asking it to say
    // so only gave it another field to get wrong.
    assert_eq!(
        status["run"]["latest_checkpoint"]["origin"].as_str(),
        Some("enriched")
    );
    client.cancel().await.expect("shutdown");
}

/// Only the two fields a checkpoint cannot be written without.
#[tokio::test]
async fn the_checkpoint_schema_asks_for_almost_nothing() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let tools = client.list_all_tools().await.expect("tools");
    let checkpoint = tools
        .iter()
        .find(|tool| tool.name == "magent_checkpoint")
        .expect("magent_checkpoint");

    let required: Vec<&str> = checkpoint
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .expect("required fields")
        .iter()
        .filter_map(Value::as_str)
        .collect();

    assert_eq!(
        required,
        ["operation_id", "stage", "handoff_summary"],
        "the checkpoint asks for more than it needs"
    );
    client.cancel().await.expect("shutdown");
}

/// Same reasoning for finishing: the run in flight is the one being finished.
#[tokio::test]
async fn finishing_needs_no_identifiers() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "fix the payment timeout" }),
    )
    .await;

    call(
        &client,
        "magent_finish",
        json!({
            "operation_id": uuid(),
            "action": "complete_run",
            "outcome": "shipped and verified"
        }),
    )
    .await;

    let status = call(&client, "magent_status", json!({})).await;
    assert_eq!(status["run"], Value::Null, "the run was completed");
    client.cancel().await.expect("shutdown");
}

/// Guessing an open run is a convenience, not a licence to invent one. An
/// explicit id still wins, and is still honoured when it names a different run.
#[tokio::test]
async fn an_explicit_run_id_still_wins() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let first = call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "first task" }),
    )
    .await;
    let run_id = first["run_id"].as_str().expect("run_id").to_owned();

    call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "run_id": run_id,
            "session_id": first["session_id"],
            "stage": "planning",
            "handoff_summary": "named explicitly"
        }),
    )
    .await;

    let status = call(&client, "magent_status", json!({})).await;
    assert_eq!(status["run"]["run_id"].as_str(), Some(run_id.as_str()));
    assert_eq!(
        status["run"]["latest_checkpoint"]["handoff_summary"].as_str(),
        Some("named explicitly")
    );
    client.cancel().await.expect("shutdown");
}

/// Checkpointing with nothing open is a mistake worth naming, not a run to
/// conjure: a run invented here would have no task and no reason to exist.
#[tokio::test]
async fn checkpointing_with_nothing_open_says_so() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let error = call_expecting_error(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "nothing is open"
        }),
    )
    .await;

    assert!(
        error.contains("no_open_run"),
        "the error should name the cause: {error}"
    );
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn a_run_survives_a_reconnect_and_returns_its_checkpoint() {
    let fixture = Fixture::new();

    let first = connect(&fixture).await;
    let started = call(
        &first,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "fix the payment timeout" }),
    )
    .await;
    let run_id = started["run_id"].as_str().expect("run_id").to_owned();

    call(
        &first,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "run_id": run_id,
            "session_id": started["session_id"],
            "stage": "executing",
            "origin": "enriched",
            "completed_steps": ["traced the owner"],
            "next_steps": ["write the regression test"],
            "decisions": ["keep the public API"],
            "rejected": ["rewriting the client"],
            "changed_files": ["src/service.rs"],
            "verification": ["targeted test is red"],
            "risks": [],
            "handoff_summary": "owner traced; regression test is next"
        }),
    )
    .await;
    first.cancel().await.expect("shutdown");

    // A fresh process, as a resumed or handed-over session would be.
    let second = connect(&fixture).await;
    let resumed = call(
        &second,
        "magent_start",
        json!({
            "operation_id": uuid(),
            "task": "fix the payment timeout",
            "resume_run_id": run_id
        }),
    )
    .await;

    assert_eq!(resumed["run_id"].as_str(), Some(run_id.as_str()));
    assert_ne!(resumed["session_id"], started["session_id"]);
    assert_eq!(
        resumed["latest_checkpoint"]["handoff_summary"].as_str(),
        Some("owner traced; regression test is next")
    );
    second.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn status_reports_the_open_run_without_a_run_id() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let empty = call(&client, "magent_status", json!({})).await;
    assert_eq!(empty["run"], Value::Null, "nothing is in flight yet");

    call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "trace the leak" }),
    )
    .await;

    let status = call(&client, "magent_status", json!({})).await;
    assert_eq!(status["run"]["task"].as_str(), Some("trace the leak"));
    client.cancel().await.expect("shutdown");
}

/// Failures reach the model as tool errors carrying a stable code, so it can
/// tell "you sent something invalid" from "this run is finished".
#[tokio::test]
async fn failures_carry_a_stable_error_code() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let message = call_expecting_error(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "   " }),
    )
    .await;

    assert!(
        message.contains("invalid_task"),
        "expected a stable code in: {message}"
    );
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn finishing_a_run_prevents_further_checkpoints() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let started = call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "ship the fix" }),
    )
    .await;

    call(
        &client,
        "magent_finish",
        json!({
            "operation_id": uuid(),
            "run_id": started["run_id"],
            "session_id": started["session_id"],
            "action": "complete_run",
            "outcome": "verified"
        }),
    )
    .await;

    let message = call_expecting_error(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "run_id": started["run_id"],
            "session_id": started["session_id"],
            "stage": "executing",
            "origin": "enriched",
            "completed_steps": [],
            "next_steps": [],
            "decisions": [],
            "rejected": [],
            "changed_files": [],
            "verification": [],
            "risks": [],
            "handoff_summary": "too late"
        }),
    )
    .await;

    assert!(message.contains("run_closed"), "got: {message}");
    client.cancel().await.expect("shutdown");
}

/// Anything on stdout that is not a protocol frame corrupts the session.
///
/// Checked against the raw stream rather than through the client, because a
/// tolerant client would skip a stray banner and hide the problem until some
/// stricter harness hit it.
#[tokio::test]
async fn stdout_carries_protocol_frames_and_nothing_else() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let fixture = Fixture::new();

    let mut child = Command::new(MAGENT)
        .arg("mcp")
        .arg("--state-dir")
        .arg(&fixture.state_dir)
        .current_dir(&fixture.repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "contract-test", "version": "0" }
        }
    });

    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(format!("{initialize}\n").as_bytes())
        .await
        .expect("write");
    stdin.flush().await.expect("flush");

    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("no response within the timeout")
        .expect("read")
        .expect("a response line");

    let parsed: Value = serde_json::from_str(&line)
        .unwrap_or_else(|error| panic!("stdout is not a JSON-RPC frame ({error}): {line:?}"));
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert_eq!(parsed["id"], 1);

    child.kill().await.expect("kill");
}

// --- memory ----------------------------------------------------------------

#[tokio::test]
async fn a_remembered_fact_can_be_searched_and_recalled() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(
        &client,
        "magent_remember",
        json!({
            "operation_id": uuid(),
            "name": "goose-table-locking",
            "title": "goose v3.26 locks with NewPostgresTableLocker",
            "body": "goose_lock is auto-created and needs DDL rights.",
            "kind": "project",
            "scope": "repository",
            "cardinality": "single",
            "status": "observed",
            "confidence": 0.8
        }),
    )
    .await;

    let found = call(
        &client,
        "magent_search",
        json!({ "text": "migration locking needs DDL rights" }),
    )
    .await;
    let facts = found["facts"].as_array().expect("facts");
    assert_eq!(facts.len(), 1, "{found}");
    assert_eq!(facts[0]["name"].as_str(), Some("goose-table-locking"));

    let recalled = call(
        &client,
        "magent_recall",
        json!({ "name": "goose-table-locking" }),
    )
    .await;
    assert!(
        recalled["fact"]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("DDL rights"),
        "recall must return the body: {recalled}"
    );
}

/// Search is what the index defers to, so an empty result has to be an ordinary
/// answer rather than an error the model has to interpret.
#[tokio::test]
async fn searching_for_nothing_returns_an_empty_list() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let found = call(
        &client,
        "magent_search",
        json!({ "text": "something nobody ever wrote down" }),
    )
    .await;

    assert_eq!(found["facts"].as_array().map(Vec::len), Some(0), "{found}");
    client.cancel().await.expect("shutdown");
}

/// The strongest status must not be the cheapest to assert.
#[tokio::test]
async fn remembering_a_verified_fact_without_evidence_is_refused() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let message = call_expecting_error(
        &client,
        "magent_remember",
        json!({
            "operation_id": uuid(),
            "name": "unchecked-claim",
            "title": "asserted without checking",
            "body": "no evidence at all",
            "kind": "project",
            "scope": "repository",
            "cardinality": "single",
            "status": "verified",
            "confidence": 1.0
        }),
    )
    .await;

    assert!(
        message.contains("verified_without_evidence"),
        "got: {message}"
    );
    client.cancel().await.expect("shutdown");
}

fn uuid() -> String {
    // Avoids a uuid dependency in the test: any distinct value works, and
    // reusing one is exactly what idempotency is meant to catch.
    format!(
        "{:08x}-0000-4000-8000-{:012x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
            % 0xffff_ffff_ffff
    )
}
