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
    ClientHandler, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, ElicitRequestParams, ElicitResult,
        ElicitationAction, ElicitationCapability,
    },
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Map, Value, json};
use tokio::process::Command;

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

/// Claude Code truncates server instructions at this size, critical detail
/// first. Exceeding it silently loses the end of the bootstrap contract.
const INSTRUCTIONS_LIMIT: usize = 2048;

/// The old "only four tools" constraint was about context cost, and tool
/// search removed it — only names and server instructions load at session
/// start — but the set still grows deliberately, one slice at a time.
const EXPECTED_TOOLS: [&str; 14] = [
    "magent_archive",
    "magent_changes",
    "magent_checkpoint",
    "magent_deps",
    "magent_finish",
    "magent_plan",
    "magent_propose",
    "magent_recall",
    "magent_remember",
    "magent_search",
    "magent_setup",
    "magent_specify",
    "magent_start",
    "magent_status",
];

/// Tools that change state. Each must take an `operation_id` so a retry after a
/// dropped connection cannot duplicate work.
///
/// `magent_setup` is here even though it only writes when asked to apply: a
/// schema cannot say "required when apply is true", and the model would learn
/// about the field from a refusal at the moment it is about to regroup
/// someone's repositories.
const MUTATING_TOOLS: [&str; 9] = [
    "magent_archive",
    "magent_checkpoint",
    "magent_finish",
    "magent_plan",
    "magent_propose",
    "magent_remember",
    "magent_setup",
    "magent_specify",
    "magent_start",
];

struct Fixture {
    dir: tempfile::TempDir,
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
            dir,
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

type Client = Connected<()>;

/// A live session with a server. Generic over the client handler, because a
/// test that answers an elicitation needs one that is not `()`.
type Connected<H> = rmcp::service::RunningService<rmcp::service::RoleClient, H>;

async fn connect(fixture: &Fixture) -> Client {
    let repo = fixture.repo.clone();
    connect_in(fixture, &repo).await
}

/// The server resolves its workspace from where it was launched, so the
/// directory is what most of these tests are really varying.
async fn connect_in(fixture: &Fixture, repo: &Path) -> Client {
    let state_dir = fixture.state_dir.clone();
    let repo = repo.to_path_buf();

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
async fn call<H: ClientHandler>(
    client: &Connected<H>,
    tool: &'static str,
    arguments: Value,
) -> Value {
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

async fn call_expecting_error<H: ClientHandler>(
    client: &Connected<H>,
    tool: &'static str,
    arguments: Value,
) -> String {
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
async fn the_server_exposes_exactly_the_expected_tools() {
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

/// The same limit against the worst payload the server can send. The test above
/// measures a workspace with nothing to say about itself, so it measures the
/// constant alone; an ungrouped one makes the server append its setup note, and
/// the note is spent out of the same 2 KB. What sizes that note is the
/// organisation it names, so this asks from a checkout whose organisation is as
/// long as a client is likely to have — `github.com` and an org name at the 39
/// characters GitHub allows. The remaining variable is the count of siblings,
/// one or two more bytes, which is what the margin below the limit is for.
///
/// A margin measured without the note is a margin that is not there, which is
/// why the note is asserted present before the length is read: a fixture that
/// stopped triggering it would leave this quietly measuring the constant.
#[tokio::test]
async fn the_bootstrap_instructions_still_fit() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "github.com/platform-infrastructure-and-tooling-eng",
        &["clients", "payments", "ledger"],
    );

    let client = connect_in(&fixture, &bank.join("clients")).await;
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("instructions");

    assert!(
        instructions.contains("Call magent_setup and offer it"),
        "no setup note, so this measures the contract alone: {instructions}"
    );

    let contract = magent_mcp::INSTRUCTIONS.len();
    assert!(
        instructions.len() <= INSTRUCTIONS_LIMIT,
        "the instructions are {} bytes — {contract} of bootstrap contract and {} of setup note — and the limit is {INSTRUCTIONS_LIMIT}",
        instructions.len(),
        instructions.len() - contract
    );
    client.cancel().await.expect("shutdown");
}

/// Five of the fourteen tools are one process, and a model that never hears
/// the process exists has no reason to go and read their descriptions. The
/// bootstrap contract is the only place it reads before deciding what to do.
#[tokio::test]
async fn the_instructions_name_the_spec_driven_process() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("instructions");

    for tool in [
        "magent_propose",
        "magent_specify",
        "magent_plan",
        "magent_archive",
        "magent_changes",
    ] {
        assert!(
            instructions.contains(tool),
            "{tool} is invisible to a model that only reads the instructions: {instructions}"
        );
    }
    client.cancel().await.expect("shutdown");
}

/// The contract's checkpoint instruction names its moment.
///
/// This guards a decision rather than a mechanism. The sentence saying when
/// to checkpoint was once deleted to buy room for a promise that the hook's
/// notice would arrive, and the contract has no room to hold both: it is
/// 1870 bytes against the 2048 at which Claude Code truncates it, and the
/// setup note spends some 126 more. The timing is worth more, because the
/// notice arrives once a session and only past the tenth edit, which leaves
/// everything before that moment to this sentence.
///
/// The absence half is a canary against that exact edit returning, not a
/// defence against a paraphrase of it — no string match could be. The
/// requirement is where the reasoning lives.
#[tokio::test]
async fn the_contract_says_when_to_checkpoint() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("instructions");

    for clause in ["at stage boundaries", "before handing work over"] {
        assert!(
            instructions.contains(clause),
            "the contract stopped saying when to checkpoint: {instructions}"
        );
    }
    assert!(
        !instructions.contains("be told"),
        "the contract promises the notice instead of saying when to call: {instructions}"
    );

    let checkpoint = client
        .list_all_tools()
        .await
        .expect("list tools")
        .into_iter()
        .find(|tool| tool.name.as_ref() == "magent_checkpoint")
        .expect("magent_checkpoint is served");
    let description = checkpoint.description.expect("a description");

    assert!(
        description.contains("at stage boundaries"),
        "the tool stopped saying when to call it: {description}"
    );
    assert!(
        !description.contains("be told"),
        "the tool promises the notice instead of saying when to call: {description}"
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

/// The key is demanded of every call, not only of the ones that apply
/// something. Spelled out as the whole required set rather than as "contains
/// the key", because the other half of the decision — that nothing else is
/// compulsory, so looking stays a call with one field — is what keeps the read
/// path cheap enough to be used.
#[tokio::test]
async fn setup_requires_an_operation_id_and_nothing_else() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let setup = client
        .list_all_tools()
        .await
        .expect("list tools")
        .into_iter()
        .find(|tool| tool.name == "magent_setup")
        .expect("magent_setup");

    let mut required: Vec<&str> = setup
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    required.sort_unstable();

    assert_eq!(required, ["operation_id"]);
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

// --- reference checkouts ----------------------------------------------------

/// The tool exists to hand over paths. Everything else the agent can do better
/// with grep and read, so the one thing this must never do is describe sources
/// without saying where they are.
#[tokio::test]
async fn deps_reports_where_the_sources_are() {
    let fixture = Fixture::new();

    let upstream = fixture.dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "t@example.invalid"],
        vec!["config", "user.name", "T"],
    ] {
        let output = std::process::Command::new("git")
            .args(&args)
            .current_dir(&upstream)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(output.status.success());
    }
    std::fs::write(upstream.join("retry.go"), "package retry\n").expect("write");
    for args in [vec!["add", "."], vec!["commit", "-m", "seed"]] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(&upstream)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
    }

    let added = std::process::Command::new(env!("CARGO_BIN_EXE_magent"))
        .args(["deps", "add", &format!("file://{}", upstream.display())])
        .current_dir(&fixture.repo)
        .env("MAGENT_STATE_DIR", &fixture.state_dir)
        .output()
        .expect("magent deps add");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let client = connect(&fixture).await;
    let reported = call(&client, "magent_deps", json!({})).await;

    let first = &reported["dependencies"][0];
    let checkout = first["path"]
        .as_str()
        .expect("a path, or the tool is pointless");
    assert_eq!(first["status"].as_str(), Some("present"));
    assert!(
        first["revision"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "a reference source is only citable at a revision: {first}"
    );
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(checkout).join("retry.go")).expect("readable"),
        "package retry\n"
    );
    assert!(
        reported["root"].as_str().is_some(),
        "the root is what makes one grep cover every dependency: {reported}"
    );
    client.cancel().await.expect("shutdown");
}

/// Reading never mutates, so it takes no `operation_id` — and an empty answer is
/// normal rather than an error.
#[tokio::test]
async fn deps_is_read_only_and_empty_is_normal() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let reported = call(&client, "magent_deps", json!({})).await;
    assert_eq!(
        reported["dependencies"].as_array().map(Vec::len),
        Some(0),
        "{reported}"
    );

    let tools = client.list_all_tools().await.expect("tools");
    let deps = tools
        .iter()
        .find(|tool| tool.name == "magent_deps")
        .expect("magent_deps");
    let required = deps
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(required, 0, "a read takes no arguments");

    client.cancel().await.expect("shutdown");
}

// --- setup ------------------------------------------------------------------

/// Builds a directory of checkouts sharing one organisation.
///
/// `organisation` is `host/org`; the scp form puts the org after the colon, and
/// getting that wrong silently gives every checkout a different organisation.
fn checkouts(parent: &Path, organisation: &str, names: &[&str]) {
    let (host, org) = organisation.split_once('/').expect("host/org");

    for name in names {
        let root = parent.join(name);
        init_repo(&root);
        let output = std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                &format!("git@{host}:{org}/{name}.git"),
            ])
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(output.status.success());
    }
}

/// The self-describing part. A server that knows it has not been set up should
/// say so where the model will read it, rather than waiting to be asked.
#[tokio::test]
async fn the_instructions_say_when_setup_is_worth_running() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments"],
    );

    let client = connect_in(&fixture, &bank.join("clients")).await;
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("instructions");

    assert!(
        instructions.contains("magent_setup"),
        "an ungrouped workspace should be mentioned: {instructions}"
    );
    assert!(
        instructions.len() <= 2048,
        "instructions grew past the truncation limit: {}",
        instructions.len()
    );
    client.cancel().await.expect("shutdown");
}

/// And should stop saying it once it is done, because an instruction that is
/// always there is one the model learns to skip.
#[tokio::test]
async fn a_settled_workspace_is_not_nagged() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments"],
    );

    let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
    store
        .group_into_workspace("fintech", &[bank.join("clients"), bank.join("payments")])
        .expect("group");
    drop(store);

    let client = connect_in(&fixture, &bank.join("clients")).await;
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("instructions");

    assert!(
        !instructions.contains("magent_setup"),
        "nothing left to set up, so nothing should be said: {instructions}"
    );
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn setup_finds_the_checkouts_that_belong_together() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments", "ledger"],
    );
    checkouts(&bank, "github.com/someone", &["blog"]);

    let client = connect_in(&fixture, &bank.join("clients")).await;
    let proposal = call(&client, "magent_setup", json!({ "operation_id": uuid() })).await;

    assert_eq!(proposal["suggested_name"].as_str(), Some("bank"));
    let found: Vec<_> = proposal["siblings"]
        .as_array()
        .expect("siblings")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(
        !found.iter().any(|root| root.ends_with("blog")),
        "unrelated work was swept in: {found:?}"
    );
    client.cancel().await.expect("shutdown");
}

/// Looking must not decide. An agent that ran setup to see what it said, and
/// thereby regrouped a person's repositories, would be worse than one that
/// never looked.
#[tokio::test]
async fn setup_without_applying_changes_nothing() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments"],
    );

    let client = connect_in(&fixture, &bank.join("clients")).await;
    call(&client, "magent_setup", json!({ "operation_id": uuid() })).await;
    client.cancel().await.expect("shutdown");

    let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
    assert!(
        store.workspaces().expect("workspaces").is_empty(),
        "a read created a group"
    );
}

/// Claude Code does not offer elicitation to MCP servers today. Applying a
/// change that regroups fifty repositories with nobody asked is not an
/// acceptable fallback, so it refuses and says what to run instead.
#[tokio::test]
async fn applying_without_a_way_to_ask_refuses_and_says_what_to_run() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments"],
    );

    let client = connect_in(&fixture, &bank.join("clients")).await;
    let refusal = call_expecting_error(
        &client,
        "magent_setup",
        json!({ "operation_id": uuid(), "apply": true }),
    )
    .await;

    assert!(
        refusal.contains("magent workspace group"),
        "the way forward has to be in the refusal: {refusal}"
    );
    client.cancel().await.expect("shutdown");

    let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
    assert!(
        store.workspaces().expect("workspaces").is_empty(),
        "it refused and grouped anyway"
    );
}

#[tokio::test]
async fn setup_says_when_there_is_nothing_to_do() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let proposal = call(&client, "magent_setup", json!({ "operation_id": uuid() })).await;

    assert_eq!(proposal["suggested_name"], Value::Null);
    assert!(
        proposal["summary"]
            .as_str()
            .is_some_and(|summary| !summary.is_empty()),
        "an empty proposal still has to explain itself: {proposal}"
    );
    client.cancel().await.expect("shutdown");
}

/// A client that answers a server's confirmation, the way a harness with a UI
/// would.
///
/// Without one of these the accept path is unreachable from a test, and the
/// whole point of asking is what happens when the answer is yes.
#[derive(Clone)]
struct Answering {
    action: ElicitationAction,
    name: Option<&'static str>,
}

impl ClientHandler for Answering {
    fn get_info(&self) -> ClientInfo {
        // Both types are #[non_exhaustive], so they are adjusted rather than
        // constructed literally.
        let mut capabilities = ClientCapabilities::default();
        capabilities.elicitation = Some(ElicitationCapability::default());

        let mut info = ClientInfo::default();
        info.capabilities = capabilities;
        info
    }

    async fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        let mut result = ElicitResult::new(self.action.clone());
        if let Some(name) = self.name {
            result = result.with_content(json!({ "name": name }));
        }
        Ok(result)
    }
}

async fn connect_answering(
    fixture: &Fixture,
    repo: &Path,
    handler: Answering,
) -> Connected<Answering> {
    let state_dir = fixture.state_dir.clone();
    let repo = repo.to_path_buf();

    let transport = TokioChildProcess::new(Command::new(MAGENT).configure(|command| {
        command
            .arg("mcp")
            .arg("--state-dir")
            .arg(&state_dir)
            .current_dir(&repo);
    }))
    .expect("spawn magent mcp");

    tokio::time::timeout(Duration::from_secs(10), handler.serve(transport))
        .await
        .expect("initialize did not time out")
        .expect("initialize")
}

/// The point of asking: when the answer is yes, the group is made — and under
/// the name the person typed, not the one that was guessed.
#[tokio::test]
async fn a_confirmed_setup_groups_the_repositories() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments", "ledger"],
    );

    let client = connect_answering(
        &fixture,
        &bank.join("clients"),
        Answering {
            action: ElicitationAction::Accept,
            name: Some("wbbank"),
        },
    )
    .await;

    let applied = call(
        &client,
        "magent_setup",
        json!({ "operation_id": uuid(), "apply": true }),
    )
    .await;
    assert_eq!(applied["grouped"].as_u64(), Some(3), "{applied}");
    assert_eq!(applied["workspace_name"].as_str(), Some("wbbank"));
    drop(client);

    let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
    let workspaces = store.workspaces().expect("workspaces");
    assert_eq!(workspaces, vec![("wbbank".to_owned(), 3)]);
}

/// Declining must leave the world exactly as it was. A confirmation that
/// changes things when refused is worse than no confirmation, because it
/// teaches people not to read it.
#[tokio::test]
async fn a_declined_setup_changes_nothing() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments"],
    );

    let client = connect_answering(
        &fixture,
        &bank.join("clients"),
        Answering {
            action: ElicitationAction::Decline,
            name: None,
        },
    )
    .await;

    let refusal = call_expecting_error(
        &client,
        "magent_setup",
        json!({ "operation_id": uuid(), "apply": true }),
    )
    .await;
    assert!(refusal.contains("declined"), "{refusal}");
    drop(client);

    let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
    assert!(store.workspaces().expect("workspaces").is_empty());
}

/// A retry must not group a second time. Grouping is idempotent on the
/// workspace name only while nothing else moved: the person may have pulled a
/// checkout out of the group between the two calls, and a replay that regrouped
/// would drag it back in — silently, on a call they thought they had already
/// answered.
#[tokio::test]
async fn a_retried_setup_does_not_group_a_second_time() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments", "ledger"],
    );

    let client = connect_answering(
        &fixture,
        &bank.join("clients"),
        Answering {
            action: ElicitationAction::Accept,
            name: Some("wbbank"),
        },
    )
    .await;

    let key = uuid();
    let applied = call(
        &client,
        "magent_setup",
        json!({ "operation_id": key.clone(), "apply": true }),
    )
    .await;
    assert_eq!(applied["grouped"].as_u64(), Some(3), "{applied}");

    // The person takes one checkout back out of the group. Regrouping would
    // undo that; replaying leaves it where they put it.
    {
        let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
        store
            .group_into_workspace("ledger-only", &[bank.join("ledger")])
            .expect("regroup");
    }

    let replayed = call(
        &client,
        "magent_setup",
        json!({ "operation_id": key, "apply": true }),
    )
    .await;
    assert_eq!(
        replayed["grouped"].as_u64(),
        Some(3),
        "a replay answers what the first call answered: {replayed}"
    );
    assert_eq!(replayed["workspace_name"].as_str(), Some("wbbank"));
    drop(client);

    let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
    assert_eq!(
        store.workspaces().expect("workspaces"),
        vec![("ledger-only".to_owned(), 1), ("wbbank".to_owned(), 2)],
        "the retry regrouped and undid what the person did in between"
    );
}

/// The same key carrying a different request is a mistake rather than a retry.
/// Here a fourth checkout appeared in between, so the second call is asking to
/// group something the first call never mentioned — and answering it from the
/// record would report a group that was never made.
#[tokio::test]
async fn a_setup_key_reused_for_a_different_request_conflicts() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("bank");
    checkouts(
        &bank,
        "gitlab.example.com/fintech",
        &["clients", "payments", "ledger"],
    );

    let client = connect_answering(
        &fixture,
        &bank.join("clients"),
        Answering {
            action: ElicitationAction::Accept,
            name: Some("wbbank"),
        },
    )
    .await;

    let key = uuid();
    call(
        &client,
        "magent_setup",
        json!({ "operation_id": key.clone(), "apply": true }),
    )
    .await;

    checkouts(&bank, "gitlab.example.com/fintech", &["treasury"]);

    let refusal = call_expecting_error(
        &client,
        "magent_setup",
        json!({ "operation_id": key, "apply": true }),
    )
    .await;
    assert!(
        refusal.contains("idempotency_conflict"),
        "a reused key with a different request has to be refused: {refusal}"
    );
    drop(client);

    let store = magent_store::Store::open(&fixture.state_dir.join("magent.db")).expect("open");
    assert_eq!(
        store.workspaces().expect("workspaces"),
        vec![("wbbank".to_owned(), 3)],
        "it refused and grouped anyway"
    );
}

// --- spec-driven work -------------------------------------------------------

/// The binding rides on the checkpoint rather than on a tool of its own. A
/// checkpoint already happens at every task boundary, which is exactly when the
/// current task changes, and a separate tool would be one more thing to forget.
#[tokio::test]
async fn a_checkpoint_can_bind_the_run_to_a_spec_change() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "add a retry budget" }),
    )
    .await;

    call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "budget type is in",
            "spec_change_id": "add-retry-budget",
            "current_task": "2: wire the budget into the client"
        }),
    )
    .await;

    let status = call(&client, "magent_status", json!({})).await;
    assert_eq!(
        status["run"]["spec"]["change_id"].as_str(),
        Some("add-retry-budget"),
        "{status}"
    );
    assert_eq!(
        status["run"]["spec"]["current_task"].as_str(),
        Some("2: wire the budget into the client")
    );
    client.cancel().await.expect("shutdown");
}

/// Advancing must take one field. Making the model restate the change every
/// time is how a run ends up bound to a task of a change it no longer names.
#[tokio::test]
async fn advancing_to_the_next_task_takes_one_field() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "add a retry budget" }),
    )
    .await;
    call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "task 1 done",
            "spec_change_id": "add-retry-budget",
            "current_task": "1"
        }),
    )
    .await;

    call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "task 2 started",
            "current_task": "2"
        }),
    )
    .await;

    let status = call(&client, "magent_status", json!({})).await;
    let spec = &status["run"]["spec"];
    assert_eq!(spec["current_task"].as_str(), Some("2"));
    assert_eq!(
        spec["change_id"].as_str(),
        Some("add-retry-budget"),
        "the change was dropped by a checkpoint that did not mention it: {status}"
    );
    client.cancel().await.expect("shutdown");
}

/// Most work is not spec-driven, and the schema must not imply otherwise.
#[tokio::test]
async fn the_spec_fields_are_all_optional() {
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
        .expect("required")
        .iter()
        .filter_map(Value::as_str)
        .collect();

    for field in ["spec_change_id", "current_task"] {
        assert!(!required.contains(&field), "{field} became required");
    }
    client.cancel().await.expect("shutdown");
}

/// What a property really says, with a `$ref` followed into `$defs` and the
/// `null` alternative of an optional field stepped over: a nested type reaches
/// the wire as a reference beside `{"type": "null"}`, and the object on the
/// other side of that is where a client reads what the type demands.
fn resolve_schema<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference
            .rsplit('/')
            .next()
            .unwrap_or_else(|| panic!("a $ref names nothing: {reference}"));
        return resolve_schema(root, &root["$defs"][name]);
    }

    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let branch = branches
            .iter()
            .find(|branch| branch.get("type").and_then(Value::as_str) != Some("null"))
            .unwrap_or_else(|| panic!("an anyOf of nothing but null: {schema}"));
        return resolve_schema(root, branch);
    }

    schema
}

/// A tick cannot omit its evidence, and the schema is where a client learns
/// that, so the property's own required list is what this asserts: settling for
/// "the property exists" would leave a caller free to send a number alone and
/// discover the rest from a refusal. The three go together because a task
/// recorded as done with nothing to check it against reads afterwards exactly
/// like one that was checked.
#[tokio::test]
async fn the_checkpoint_tool_takes_a_tick() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let tools = client.list_all_tools().await.expect("tools");
    let checkpoint = tools
        .iter()
        .find(|tool| tool.name == "magent_checkpoint")
        .expect("magent_checkpoint");
    let root = Value::Object(checkpoint.input_schema.as_ref().clone());

    let property = &root["properties"]["task_done"];
    assert!(
        !property.is_null(),
        "a checkpoint cannot close a task: {root}"
    );

    let mut required: Vec<&str> = resolve_schema(&root, property)["required"]
        .as_array()
        .unwrap_or_else(|| panic!("task_done requires nothing: {property}"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    required.sort_unstable();

    assert_eq!(
        required,
        ["number", "output", "verify_command"],
        "a tick that can leave out its evidence: {root}"
    );
    client.cancel().await.expect("shutdown");
}

// --- propose and specify ----------------------------------------------------

/// Sorted required-field names for one tool's input schema.
async fn required_fields<H: ClientHandler>(client: &Connected<H>, tool: &str) -> Vec<String> {
    let tools = client.list_all_tools().await.expect("tools");
    let found = tools
        .iter()
        .find(|candidate| candidate.name == tool)
        .unwrap_or_else(|| panic!("{tool} was not found among the server's tools"));

    let mut required: Vec<String> = found
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    required.sort();
    required
}

/// Only the fields a proposal is meaningless without. `what_changes` and
/// `capabilities` are lists a call can reasonably send empty — the latter is
/// how a change declares `skip_specs` and leaves the domain layer to say so —
/// so neither belongs in `required`.
#[tokio::test]
async fn the_propose_schema_asks_for_only_what_a_proposal_needs() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let required = required_fields(&client, "magent_propose").await;

    assert_eq!(
        required,
        ["classification", "operation_id", "slug", "title", "why"],
        "magent_propose asks for more or less than it needs"
    );
    client.cancel().await.expect("shutdown");
}

/// `change`, `capability_path` and `requirements` are all load-bearing: a
/// specify call naming none of them has nothing to attach and nowhere to
/// attach it. `purpose` stays optional — required only for a capability that
/// is new, which only the store can tell.
#[tokio::test]
async fn the_specify_schema_asks_for_only_what_a_delta_needs() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let required = required_fields(&client, "magent_specify").await;

    assert_eq!(
        required,
        ["capability_path", "change", "operation_id", "requirements"],
        "magent_specify asks for more or less than it needs"
    );
    client.cancel().await.expect("shutdown");
}

/// Proposing a change and then specifying one of its capabilities, end to
/// end, through the real server.
#[tokio::test]
async fn a_proposed_change_can_be_specified() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let proposed = call(
        &client,
        "magent_propose",
        json!({
            "operation_id": uuid(),
            "slug": "add-retry-budget",
            "title": "Add a retry budget",
            "classification": "bounded",
            "why": "retries currently loop forever and starve the queue of capacity",
            "what_changes": ["add a budget type", "wire it into the client"],
            "capabilities": ["worker/retry"]
        }),
    )
    .await;
    let change_id = proposed["id"].as_str().expect("id").to_owned();

    let specified = call(
        &client,
        "magent_specify",
        json!({
            "operation_id": uuid(),
            "change": change_id,
            "capability_path": "worker/retry",
            "purpose": "Retries stop after a configured budget instead of continuing forever, so a failing dependency cannot starve the queue of capacity for other work.",
            "requirements": [{
                "op": "added",
                "name": "budget-caps-retries",
                "text": "A retry budget caps the number of attempts a worker makes before giving up.",
                "scenarios": [{
                    "name": "exceeding the budget stops retrying",
                    "when": "a job has already failed budget times",
                    "then": "the worker does not retry it again"
                }]
            }]
        }),
    )
    .await;

    assert_eq!(specified["capability_path"].as_str(), Some("worker/retry"));
    assert_eq!(specified["added"].as_u64(), Some(1), "{specified}");
    assert_eq!(specified["modified"].as_u64(), Some(0));
    assert_eq!(specified["removed"].as_u64(), Some(0));
    assert_eq!(specified["renamed"].as_u64(), Some(0));
    assert_eq!(specified["status"].as_str(), Some("specified"));
    client.cancel().await.expect("shutdown");
}

/// A domain failure has to reach the model as something it can act on: a
/// stable code to branch on, and prose to explain it. Omitting both
/// `capabilities` and `skip_specs` is refused by `ProposeCommand::validate`,
/// not by the JSON-Schema layer — which is only true because neither field is
/// required in the schema.
#[tokio::test]
async fn proposing_without_capabilities_or_skip_specs_says_so() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let error = call_expecting_error(
        &client,
        "magent_propose",
        json!({
            "operation_id": uuid(),
            "slug": "add-something",
            "title": "Add something",
            "classification": "bounded",
            "why": "needed for reasons a reviewer would accept"
        }),
    )
    .await;

    assert!(
        error.contains("missing_capabilities"),
        "expected the stable domain code in: {error}"
    );
    client.cancel().await.expect("shutdown");
}

// --- plan, archive and reading back ------------------------------------------

/// One proposal, ready to be carried through the rest of the process.
fn proposal(slug: &str) -> Value {
    json!({
        "operation_id": uuid(),
        "slug": slug,
        "title": "Add a retry budget",
        "classification": "bounded",
        "why": "retries currently loop forever and starve the queue of capacity",
        "what_changes": ["add a budget type", "wire it into the client"],
        "capabilities": ["worker/retry"]
    })
}

/// One capability's deltas for the change addressed by `change`.
fn deltas(change: &str) -> Value {
    json!({
        "operation_id": uuid(),
        "change": change,
        "capability_path": "worker/retry",
        "purpose": "Retries stop after a configured budget instead of continuing forever, so a failing dependency cannot starve the queue of capacity for other work.",
        "requirements": [{
            "op": "added",
            "name": "budget-caps-retries",
            "text": "A retry budget caps the number of attempts a worker makes before giving up.",
            "scenarios": [{
                "name": "exceeding the budget stops retrying",
                "when": "a job has already failed budget times",
                "then": "the worker does not retry it again"
            }]
        }]
    })
}

/// A plan covering the requirement those deltas propose.
fn tasks(change: &str) -> Value {
    json!({
        "operation_id": uuid(),
        "change": change,
        "tasks": [{
            "number": "1",
            "title": "cap the attempts in the worker",
            "verify_command": "cargo test -p worker budget",
            "expected_output": ["test budget_caps_retries ... ok"],
            "covers": ["budget-caps-retries"]
        }]
    })
}

/// Closes every task of the fixture's database, the way executing a plan would
/// leave them.
///
/// The rows are set directly, as `magent-store`'s own archive tests do, rather
/// than ticked through `magent_checkpoint`: a tick needs a run bound to the
/// change and a plan whose commands it can quote, which is what
/// `a_tick_over_mcp_closes_the_task` exists to exercise. A test about archiving
/// only needs the tasks out of its way.
fn finish_tasks(fixture: &Fixture) {
    let connection =
        rusqlite::Connection::open(fixture.state_dir.join("magent.db")).expect("open the store");
    connection
        .busy_timeout(Duration::from_secs(10))
        .expect("busy timeout");
    connection
        .execute("UPDATE tasks SET status = 'done'", [])
        .expect("finish the tasks");
}

/// Only the fields a plan and an archive are meaningless without. Everything
/// else about a change — which capabilities it touches, what its tasks cover —
/// the server reads from the change itself.
#[tokio::test]
async fn the_plan_and_archive_schemas_ask_for_only_what_they_need() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    assert_eq!(
        required_fields(&client, "magent_plan").await,
        ["change", "operation_id", "tasks"],
        "magent_plan asks for more or less than it needs"
    );
    assert_eq!(
        required_fields(&client, "magent_archive").await,
        ["change", "operation_id"],
        "magent_archive asks for more or less than it needs"
    );
    client.cancel().await.expect("shutdown");
}

/// What a task cannot be planned without, read off the schema a planner is
/// handed rather than off the refusal it would otherwise meet. `expected_output`
/// belongs there: `Store::plan` turns away a task that names no marker, and a
/// schema advertising the field as optional would send a planner to argue with
/// the wrong layer. The list is asserted whole, because a field quietly leaving
/// it is the same mistake in the other direction.
#[tokio::test]
async fn the_plan_schema_asks_every_task_for_its_expected_output() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let tools = client.list_all_tools().await.expect("tools");
    let plan = tools
        .iter()
        .find(|tool| tool.name == "magent_plan")
        .expect("magent_plan");
    let root = Value::Object(plan.input_schema.as_ref().clone());

    let tasks = resolve_schema(&root, &root["properties"]["tasks"]);
    let draft = resolve_schema(&root, &tasks["items"]);
    let mut required: Vec<&str> = draft["required"]
        .as_array()
        .unwrap_or_else(|| panic!("a task requires nothing: {draft}"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    required.sort_unstable();

    assert_eq!(
        required,
        ["expected_output", "number", "title", "verify_command"],
        "a task the schema lets a planner send incomplete: {draft}"
    );
    client.cancel().await.expect("shutdown");
}

/// Reading never mutates, so there is nothing for an `operation_id` to make
/// idempotent — and asking for one would imply the call changes something.
#[tokio::test]
async fn reading_changes_takes_no_operation_id() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    assert!(
        required_fields(&client, "magent_changes").await.is_empty(),
        "a read takes no required arguments"
    );

    let tools = client.list_all_tools().await.expect("tools");
    let changes = tools
        .iter()
        .find(|tool| tool.name == "magent_changes")
        .expect("magent_changes");
    let properties = changes
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("properties");

    assert!(
        !properties.contains_key("operation_id"),
        "magent_changes offers an operation_id, so it looks like it mutates: {:?}",
        properties.keys().collect::<Vec<_>>()
    );
    client.cancel().await.expect("shutdown");
}

/// The whole process end to end, addressing the change by the slug its author
/// chose rather than by an identifier the server issued. A model that has lost
/// the identifier to a compaction still knows the slug it invented itself.
#[tokio::test]
async fn a_change_goes_from_proposal_to_archive_addressed_by_slug() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let proposed = call(&client, "magent_propose", proposal("add-retry-budget")).await;
    assert_eq!(proposed["slug"].as_str(), Some("add-retry-budget"));

    let specified = call(&client, "magent_specify", deltas("add-retry-budget")).await;
    assert_eq!(
        specified["status"].as_str(),
        Some("specified"),
        "{specified}"
    );

    let planned = call(&client, "magent_plan", tasks("add-retry-budget")).await;
    assert_eq!(planned["tasks"].as_u64(), Some(1), "{planned}");
    assert_eq!(planned["status"].as_str(), Some("planned"));

    finish_tasks(&fixture);

    let archived = call(
        &client,
        "magent_archive",
        json!({ "operation_id": uuid(), "change": "add-retry-budget" }),
    )
    .await;
    assert_eq!(archived["status"].as_str(), Some("archived"), "{archived}");
    assert_eq!(archived["added"].as_u64(), Some(1));
    assert_eq!(
        archived["capabilities_created"][0].as_str(),
        Some("worker/retry"),
        "the archive folded the delta into a capability that did not exist: {archived}"
    );
    client.cancel().await.expect("shutdown");
}

/// The gap the dogfooding run found: the process wrote but could not be read
/// back. Without an argument it says what is open; with one it hands back the
/// deltas and the tasks filed under that change.
#[tokio::test]
async fn changes_lists_what_is_open_and_opens_one_by_slug() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(&client, "magent_propose", proposal("add-retry-budget")).await;
    call(&client, "magent_specify", deltas("add-retry-budget")).await;
    call(&client, "magent_plan", tasks("add-retry-budget")).await;

    let listed = call(&client, "magent_changes", json!({})).await;
    let open = listed["open"].as_array().expect("open changes");
    assert_eq!(open.len(), 1, "{listed}");
    assert_eq!(open[0]["slug"].as_str(), Some("add-retry-budget"));
    assert_eq!(open[0]["status"].as_str(), Some("planned"));
    assert_eq!(
        listed["change"],
        Value::Null,
        "nothing was asked about in particular: {listed}"
    );

    let opened = call(
        &client,
        "magent_changes",
        json!({ "change": "add-retry-budget" }),
    )
    .await;
    let change = &opened["change"];
    assert_eq!(change["slug"].as_str(), Some("add-retry-budget"));
    assert_eq!(
        change["deltas"][0]["name"].as_str(),
        Some("budget-caps-retries"),
        "{opened}"
    );
    assert_eq!(
        change["tasks"][0]["verify_command"].as_str(),
        Some("cargo test -p worker budget"),
        "{opened}"
    );
    client.cancel().await.expect("shutdown");
}

/// Carries one change all the way to `archived`, which is the only thing that
/// puts a capability into the live specification: until a change is folded in,
/// its requirements are deltas of a proposal and nothing reads them back as
/// what is currently true.
async fn archive_one_capability(client: &Client, fixture: &Fixture) {
    call(client, "magent_propose", proposal("add-retry-budget")).await;
    call(client, "magent_specify", deltas("add-retry-budget")).await;
    call(client, "magent_plan", tasks("add-retry-budget")).await;
    finish_tasks(fixture);
    call(
        client,
        "magent_archive",
        json!({ "operation_id": uuid(), "change": "add-retry-budget" }),
    )
    .await;
}

/// The index comes on every answer, unasked. A model that has just been handed
/// what is open still has to know what the specification already covers before
/// it proposes anything, and a second call to find that out is a call it will
/// not make.
#[tokio::test]
async fn changes_carries_the_capability_index() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    archive_one_capability(&client, &fixture).await;

    let listed = call(&client, "magent_changes", json!({})).await;
    let capabilities = listed["capabilities"].as_array().expect("capabilities");

    assert_eq!(capabilities.len(), 1, "{listed}");
    assert_eq!(capabilities[0]["path"].as_str(), Some("worker/retry"));
    assert_eq!(
        capabilities[0]["requirement_count"].as_u64(),
        Some(1),
        "the archived delta is one live requirement: {listed}"
    );
    assert_eq!(
        listed["capability"],
        Value::Null,
        "nothing was asked about in particular: {listed}"
    );
    client.cancel().await.expect("shutdown");
}

/// Naming a capability reads the live specification back — the text of each
/// requirement and the scenarios that make it checkable, not the names alone.
/// This is what `archive` exists to produce, and until now nothing consumed it.
#[tokio::test]
async fn changes_reads_one_capability_in_full() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    archive_one_capability(&client, &fixture).await;

    let read = call(
        &client,
        "magent_changes",
        json!({ "capability": "worker/retry" }),
    )
    .await;
    let capability = &read["capability"];

    assert_eq!(capability["path"].as_str(), Some("worker/retry"), "{read}");
    let requirement = &capability["requirements"][0];
    assert_eq!(
        requirement["name"].as_str(),
        Some("budget-caps-retries"),
        "{read}"
    );
    assert_eq!(
        requirement["text"].as_str(),
        Some("A retry budget caps the number of attempts a worker makes before giving up."),
        "the name without the text is not the specification: {read}"
    );
    let scenario = &requirement["scenarios"][0];
    assert_eq!(
        scenario["when"].as_str(),
        Some("a job has already failed budget times"),
        "{read}"
    );
    assert_eq!(
        scenario["then"].as_str(),
        Some("the worker does not retry it again"),
        "{read}"
    );
    client.cancel().await.expect("shutdown");
}

/// A path nobody has is a mistake the caller can fix, and the index is what
/// tells it which path it meant — the same courtesy an unknown slug gets. An
/// error here would be a refusal to answer a legitimate question.
#[tokio::test]
async fn an_unknown_capability_still_gets_the_index() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    archive_one_capability(&client, &fixture).await;

    let read = call(
        &client,
        "magent_changes",
        json!({ "capability": "worker/retyr" }),
    )
    .await;

    assert_eq!(
        read["capability"],
        Value::Null,
        "there is nothing under that path: {read}"
    );
    assert_eq!(
        read["capabilities"][0]["path"].as_str(),
        Some("worker/retry"),
        "the index has to name what the caller should have asked for: {read}"
    );
    client.cancel().await.expect("shutdown");
}

/// A slug nobody has is a mistake the caller can fix, but only if it is told
/// what does exist. Sending it away to look for itself is what made the
/// identifier-only addressing painful in the first place.
#[tokio::test]
async fn an_unknown_slug_names_the_open_ones() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(&client, "magent_propose", proposal("add-retry-budget")).await;

    let error = call_expecting_error(
        &client,
        "magent_archive",
        json!({ "operation_id": uuid(), "change": "add-retyr-budget" }),
    )
    .await;

    assert!(
        error.contains("change_not_found"),
        "expected a stable code in: {error}"
    );
    assert!(
        error.contains("add-retry-budget"),
        "the refusal has to name what is open: {error}"
    );
    client.cancel().await.expect("shutdown");
}

/// Proposing the same slug twice rewrites the proposal, and that is the
/// ordinary way to widen a change's scope. A bare identifier back would leave
/// the caller unable to tell that from having opened something new.
#[tokio::test]
async fn proposing_the_same_slug_again_reports_a_rewrite() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    let first = call(&client, "magent_propose", proposal("add-retry-budget")).await;
    assert_eq!(first["status"].as_str(), Some("drafting"), "{first}");
    assert_eq!(
        first["rewritten"].as_bool(),
        Some(false),
        "nothing stood under this slug: {first}"
    );

    let again = call(&client, "magent_propose", proposal("add-retry-budget")).await;
    assert_eq!(
        again["rewritten"].as_bool(),
        Some(true),
        "a rewrite has to say it rewrote: {again}"
    );
    assert_eq!(
        again["id"].as_str(),
        first["id"].as_str(),
        "a rewrite keeps the change it rewrote: {again}"
    );
    client.cancel().await.expect("shutdown");
}

// --- closing a task ----------------------------------------------------------

/// The loop, over the wire: a plan is written, a run is opened on it, and one
/// checkpoint both binds the run to the change and closes the first task with
/// the command the plan named. One message, because the binding is applied
/// before the tick in the same transaction — an agent that has just finished
/// task 1 has nothing else to send first.
#[tokio::test]
async fn a_tick_over_mcp_closes_the_task() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(&client, "magent_propose", proposal("add-retry-budget")).await;
    call(&client, "magent_specify", deltas("add-retry-budget")).await;
    call(&client, "magent_plan", tasks("add-retry-budget")).await;
    call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "add a retry budget" }),
    )
    .await;

    let saved = call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "the worker stops once the budget is spent",
            "spec_change_id": "add-retry-budget",
            "current_task": "1: cap the attempts in the worker",
            "task_done": {
                "number": "1",
                "verify_command": "cargo test -p worker budget",
                "output": "running 1 test\ntest budget_caps_retries ... ok\n"
            }
        }),
    )
    .await;

    let task = &saved["task"];
    assert_eq!(
        task["number"].as_str(),
        Some("1"),
        "the tick closed nothing: {saved}"
    );
    assert_eq!(
        task["expected_output_missing"].as_array(),
        Some(&vec![]),
        "the plan expected that line and the output carries it: {saved}"
    );
    assert_eq!(
        task["change_ready"].as_bool(),
        Some(true),
        "this plan has one task, so closing it leaves none open: {saved}"
    );
    client.cancel().await.expect("shutdown");
}

// --- the loop, end to end ----------------------------------------------------

/// The proposal the loop test carries the whole way: one capability, two
/// requirements, two tasks. Held here rather than inline so the test below
/// reads as the narrative it is meant to be.
fn loop_proposal() -> Value {
    json!({
        "operation_id": uuid(),
        "slug": "bound-the-retries",
        "title": "Bound the retries",
        "classification": "bounded",
        "why": "retries currently loop forever and starve the queue of capacity",
        "what_changes": ["cap the attempts", "spend the budget per attempt"],
        "capabilities": ["worker/retry"]
    })
}

/// Its two requirements, whose names sort the other way round from the order
/// they are specified in — so the order the specification reads back in is the
/// one `capability_detail` documents rather than the one this test wrote.
fn loop_deltas() -> Value {
    json!({
        "operation_id": uuid(),
        "change": "bound-the-retries",
        "capability_path": "worker/retry",
        "purpose": "Retries stop after a configured budget instead of continuing forever, so a failing dependency cannot starve the queue of capacity for other work.",
        "requirements": [
            {
                "op": "added",
                "name": "budget-caps-retries",
                "text": "A retry budget caps the number of attempts a worker makes before giving up.",
                "scenarios": [{
                    "name": "exceeding the budget stops retrying",
                    "given": "a job that has already failed budget times",
                    "when": "the worker considers it again",
                    "then": "the job is parked instead of retried"
                }]
            },
            {
                "op": "added",
                "name": "attempts-spend-the-budget",
                "text": "Each attempt spends one unit of the job's budget.",
                "scenarios": [{
                    "name": "an attempt is charged",
                    "when": "an attempt fails",
                    "then": "the job's remaining budget is one lower"
                }]
            }
        ]
    })
}

/// Its plan: one task per requirement, each with the command that will close it
/// and what that command prints, and the second consuming what the first
/// promises to produce.
fn loop_tasks() -> Value {
    json!({
        "operation_id": uuid(),
        "change": "bound-the-retries",
        "tasks": [
            {
                "number": "1",
                "title": "cap the attempts in the worker",
                "body": "Read the budget from config and refuse a retry once it is spent.",
                "files": ["crates/worker/src/retry.rs"],
                "produces": "fn spend_budget(&mut self) -> bool",
                "verify_command": "cargo test -p worker budget",
                "expected_output": ["test budget_caps_retries ... ok"],
                "covers": ["budget-caps-retries"]
            },
            {
                "number": "2",
                "title": "charge every attempt against the budget",
                "body": "Call spend_budget from the attempt path, once per attempt.",
                "files": ["crates/worker/src/attempt.rs"],
                "consumes": "fn spend_budget(&mut self) -> bool, from task 1",
                "verify_command": "cargo test -p worker attempt",
                "expected_output": ["test attempts_spend_the_budget ... ok"],
                "covers": ["attempts-spend-the-budget"]
            }
        ]
    })
}

/// The seam itself, as the subject.
///
/// Every gap this change closes was found by running the loop and none by
/// review, because each review looked at one piece and the holes were between
/// them. So this drives the whole process through the real server: propose,
/// specify, plan, bind, tick each task with the command its plan named, and
/// archive — with the refusal that would have caught the original hole asserted
/// in the middle, at the one moment it can be provoked.
///
/// Long on purpose, and split no further than the payloads above: the subject
/// is the order of the steps, and a step lifted into a helper is a step whose
/// place in that order stops being visible here.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one continuous loop narrative")]
async fn the_whole_loop_closes() {
    let fixture = Fixture::new();
    let client = connect(&fixture).await;

    call(&client, "magent_propose", loop_proposal()).await;

    let specified = call(&client, "magent_specify", loop_deltas()).await;
    assert_eq!(specified["added"].as_u64(), Some(2), "{specified}");

    let planned = call(&client, "magent_plan", loop_tasks()).await;
    assert_eq!(planned["tasks"].as_u64(), Some(2), "{planned}");
    assert_eq!(planned["status"].as_str(), Some("planned"));

    // The negative in the middle, and the reason this test exists: before this
    // change, `planned` was where the process stopped. Archiving here has to be
    // refused, and the refusal has to name what is still open — otherwise the
    // caller is told the loop is stuck and not where.
    let refused = call_expecting_error(
        &client,
        "magent_archive",
        json!({ "operation_id": uuid(), "change": "bound-the-retries" }),
    )
    .await;
    assert!(
        refused.contains("change_not_executed"),
        "expected the stable code in: {refused}"
    );
    for number in ["1", "2"] {
        assert!(
            refused.contains(number),
            "the refusal has to name task {number}: {refused}"
        );
    }

    // What the agent about to do task 1 reads: its own task, whole, including
    // the body nobody else is left to hand it after a compaction.
    let mid_flight = call(
        &client,
        "magent_changes",
        json!({ "change": "bound-the-retries" }),
    )
    .await;
    let task_one = &mid_flight["change"]["tasks"][0];
    assert_eq!(task_one["status"].as_str(), Some("pending"), "{mid_flight}");
    assert_eq!(
        task_one["body"].as_str(),
        Some("Read the budget from config and refuse a retry once it is spent."),
        "{mid_flight}"
    );
    assert_eq!(
        task_one["expected_output"],
        json!(["test budget_caps_retries ... ok"])
    );
    assert_eq!(
        mid_flight["change"]["deltas"][0]["text"].as_str(),
        Some("Each attempt spends one unit of the job's budget."),
        "a reviewer reads the requirement, not only its name: {mid_flight}"
    );

    call(
        &client,
        "magent_start",
        json!({ "operation_id": uuid(), "task": "bound the retries" }),
    )
    .await;

    let first = call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "the worker refuses a retry once the budget is spent",
            "spec_change_id": "bound-the-retries",
            "current_task": "2: charge every attempt against the budget",
            "task_done": {
                "number": "1",
                "verify_command": "cargo test -p worker budget",
                "output": "running 1 test\ntest budget_caps_retries ... ok\n"
            }
        }),
    )
    .await;
    assert_eq!(
        first["task"]["expected_output_missing"].as_array(),
        Some(&vec![]),
        "the plan expected that line and the output carries it: {first}"
    );
    assert_eq!(
        first["task"]["change_ready"].as_bool(),
        Some(false),
        "task 2 is still open: {first}"
    );

    // The binding rode in with the first tick, so this one carries the tick
    // alone — which is the whole point of not making it restate the change.
    let second = call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": uuid(),
            "stage": "executing",
            "handoff_summary": "every attempt is charged against the budget",
            "task_done": {
                "number": "2",
                "verify_command": "cargo test -p worker attempt",
                "output": "running 1 test\ntest attempts_spend_the_budget ... ok\n"
            }
        }),
    )
    .await;
    assert_eq!(
        second["task"]["change_ready"].as_bool(),
        Some(true),
        "the last open task closed, so the change is ready: {second}"
    );

    let ticked = call(
        &client,
        "magent_changes",
        json!({ "change": "bound-the-retries" }),
    )
    .await;
    let tasks = ticked["change"]["tasks"].as_array().expect("tasks");
    for task in tasks {
        assert_eq!(task["status"].as_str(), Some("done"), "{ticked}");
        assert!(
            task["evidence"]
                .as_str()
                .is_some_and(|evidence| evidence.contains("... ok")),
            "a tick is worth no more than the evidence under it: {task}"
        );
        assert!(task["verified_at"].as_str().is_some(), "{task}");
    }

    let archived = call(
        &client,
        "magent_archive",
        json!({ "operation_id": uuid(), "change": "bound-the-retries" }),
    )
    .await;
    assert_eq!(archived["status"].as_str(), Some("archived"), "{archived}");
    assert_eq!(archived["added"].as_u64(), Some(2));
    assert_eq!(
        archived["capabilities_created"][0].as_str(),
        Some("worker/retry")
    );

    // And what the next session sees: nothing in flight, and a specification
    // that now says what this change proposed.
    let after = call(&client, "magent_changes", json!({})).await;
    assert_eq!(
        after["open"].as_array().map(Vec::len),
        Some(0),
        "the change is archived, so nothing is open: {after}"
    );
    assert_eq!(
        after["capabilities"][0]["path"].as_str(),
        Some("worker/retry")
    );
    assert_eq!(
        after["capabilities"][0]["requirement_count"].as_u64(),
        Some(2),
        "{after}"
    );

    let capability = call(
        &client,
        "magent_changes",
        json!({ "capability": "worker/retry" }),
    )
    .await;
    let names: Vec<&str> = capability["capability"]["requirements"]
        .as_array()
        .expect("requirements")
        .iter()
        .filter_map(|requirement| requirement["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["attempts-spend-the-budget", "budget-caps-retries"],
        "both requirements are live: {capability}"
    );
    assert_eq!(
        capability["capability"]["requirements"][1]["scenarios"][0]["given"].as_str(),
        Some("a job that has already failed budget times"),
        "the scenarios came through the whole loop: {capability}"
    );
    client.cancel().await.expect("shutdown");
}
