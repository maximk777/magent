//! The slice-1 hypothesis, end to end.
//!
//! *After a compaction and after `/clear`, work continues without the user
//! retelling it.*
//!
//! Driven through the two surfaces a harness actually uses — the hook binary
//! and the MCP server — rather than through the store. A test that reached into
//! the store would pass even if the wiring between them were broken, which is
//! the only thing this file exists to check.

use std::{
    io::Write,
    path::Path,
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

struct Session {
    state_dir: std::path::PathBuf,
    repo: std::path::PathBuf,
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
            "{event} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn init_repo(root: &Path) {
    std::fs::create_dir_all(root).expect("mkdir");
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "t@example.invalid"],
        vec!["config", "user.name", "T"],
    ] {
        Command::new("git")
            .args(&args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
    }
    std::fs::write(root.join("README.md"), "seed\n").expect("write");
    for args in [vec!["add", "."], vec!["commit", "-m", "seed"]] {
        Command::new("git")
            .args(&args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
    }
}

type Client = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

async fn connect(state_dir: &Path, repo: &Path) -> Client {
    let state_dir = state_dir.to_path_buf();
    let repo = repo.to_path_buf();

    let transport =
        TokioChildProcess::new(tokio::process::Command::new(MAGENT).configure(|command| {
            command
                .arg("mcp")
                .arg("--state-dir")
                .arg(&state_dir)
                .current_dir(&repo);
        }))
        .expect("spawn mcp");

    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .expect("initialize in time")
        .expect("initialize")
}

async fn call(client: &Client, tool: &'static str, arguments: Value) -> Value {
    let mut params = CallToolRequestParams::default();
    params.name = tool.into();
    params.arguments = Some(arguments.as_object().cloned().unwrap_or_else(Map::new));

    let result = tokio::time::timeout(Duration::from_secs(10), client.call_tool(params))
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

fn operation_id(tag: u32) -> String {
    format!("00000000-0000-4000-8000-{tag:012x}")
}

/// The whole slice, in the order a real session lives it.
///
/// Kept as one linear narrative rather than split into helpers: the point of
/// this test is that it reads top to bottom the way a session is actually
/// lived, and hiding the steps behind names would defeat that.
#[expect(clippy::too_many_lines, reason = "one continuous session narrative")]
#[tokio::test]
async fn work_survives_compaction_and_clear() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("state");
    let repo = dir.path().join("project");
    std::fs::create_dir_all(&state_dir).expect("mkdir");
    init_repo(&repo);

    let first = Session {
        state_dir: state_dir.clone(),
        repo: repo.clone(),
        id: "session-one".into(),
    };

    // 1. A fresh session in a workspace with nothing in flight says nothing.
    assert!(
        first
            .hook("session-start", &json!({"startup_reason": "startup"}))
            .trim()
            .is_empty(),
        "an empty workspace must not spend context announcing itself"
    );

    // 2. The user asks for something. The run exists from here on, whether or
    //    not the model ever announces it.
    first.hook(
        "user-prompt-submit",
        &json!({"prompt": "make the retry budget configurable per client"}),
    );

    // 3. The model opens the run it is already in, rather than a second one.
    let client = connect(&state_dir, &repo).await;
    let started = call(
        &client,
        "magent_start",
        json!({
            "operation_id": operation_id(1),
            "task": "make the retry budget configurable per client"
        }),
    )
    .await;
    let run_id = started["run_id"].as_str().expect("run_id").to_owned();

    let status = call(&client, "magent_status", json!({})).await;
    assert_eq!(
        status["run"]["run_id"].as_str(),
        Some(run_id.as_str()),
        "the hook and the model must converge on one run, not two"
    );

    // 4. Work happens. Edits are captured without the model reporting them.
    let edited = repo.join("src/client.rs");
    std::fs::create_dir_all(edited.parent().expect("parent")).expect("mkdir");
    std::fs::write(&edited, "pub const RETRIES: u32 = 3;\n").expect("write");
    first.hook(
        "post-tool-use",
        &json!({"tool_name": "Edit", "tool_input": {"file_path": edited}}),
    );

    // 5. The model records what only it knows.
    call(
        &client,
        "magent_checkpoint",
        json!({
            "operation_id": operation_id(2),
            "run_id": run_id,
            "session_id": started["session_id"],
            "stage": "executing",
            "origin": "enriched",
            "completed_steps": ["found the hardcoded budget in src/client.rs"],
            "next_steps": ["thread the value through the config loader"],
            "decisions": ["make it configurable rather than raising the default"],
            "rejected": ["raising the default, which hides the latency problem"],
            "changed_files": ["src/client.rs"],
            "verification": ["the existing retry test still passes"],
            "risks": [],
            "handoff_summary": "budget located and made configurable; config wiring is next"
        }),
    )
    .await;
    client.cancel().await.expect("shutdown");

    // 6. Context fills up and is compacted.
    assert!(
        first
            .hook("pre-compact", &json!({"compaction_reason": "auto"}))
            .is_empty(),
        "pre-compact must not write to stdout; it cannot inject"
    );

    // 7. The session resumes on the far side of the compaction.
    let packet = first.hook("session-start", &json!({"startup_reason": "compact"}));
    assert_context_restored(&packet, &run_id);
    assert!(
        packet.contains("raising the default"),
        "a rejected alternative must survive, or it gets re-proposed:\n{packet}"
    );
    assert!(
        packet.contains("src/client.rs"),
        "the observed edit must survive:\n{packet}"
    );

    // 8. The user clears the context entirely, in a brand new session.
    let second = Session {
        state_dir: state_dir.clone(),
        repo: repo.clone(),
        id: "session-two".into(),
    };
    let after_clear = second.hook("session-start", &json!({"startup_reason": "clear"}));
    assert_context_restored(&after_clear, &run_id);

    // 9. The work is finished, deliberately.
    let client = connect(&state_dir, &repo).await;
    let finished = call(
        &client,
        "magent_finish",
        json!({
            "operation_id": operation_id(3),
            "run_id": run_id,
            "session_id": started["session_id"],
            "action": "complete_run",
            "outcome": "verified"
        }),
    )
    .await;
    assert_eq!(finished["status"], "completed");
    client.cancel().await.expect("shutdown");

    // 10. A completed run stops being restored: it is no longer in flight.
    let third = Session {
        state_dir,
        repo,
        id: "session-three".into(),
    };
    assert!(
        third
            .hook("session-start", &json!({"startup_reason": "startup"}))
            .trim()
            .is_empty(),
        "a finished task must not keep announcing itself"
    );
}

fn assert_context_restored(packet: &str, run_id: &str) {
    assert!(
        packet.contains(run_id),
        "the packet must name the run:\n{packet}"
    );
    assert!(
        packet.contains("make the retry budget configurable"),
        "the packet must carry the task:\n{packet}"
    );
    assert!(
        packet.contains("config wiring is next"),
        "the packet must carry what to do next:\n{packet}"
    );
}
