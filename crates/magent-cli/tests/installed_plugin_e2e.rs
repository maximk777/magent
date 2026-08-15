//! Running the plugin the way Claude Code runs it.
//!
//! `plugin_manifest.rs` checks what the manifests say. This runs what they say:
//! it stages an installed plugin in a temporary directory, sets
//! `CLAUDE_PLUGIN_ROOT` to it, and executes the command strings verbatim,
//! through a shell, as the harness does.
//!
//! Both defects that reached a live install were of this shape — a command that
//! resolved during development and nowhere else, and a manifest that was never
//! committed. Neither would have survived this file.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::{Value, json};

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

/// The repository's `plugin/` directory, as committed.
fn source_plugin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugin")
        .canonicalize()
        .expect("the plugin directory")
}

/// A plugin staged the way an install leaves it: the committed manifests plus a
/// built binary at `bin/magent`.
struct Installed {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state_dir: PathBuf,
    repo: PathBuf,
}

impl Installed {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("plugin");
        let state_dir = dir.path().join("state");
        let repo = dir.path().join("project");

        std::fs::create_dir_all(root.join("bin")).expect("mkdir");
        std::fs::create_dir_all(root.join("hooks")).expect("mkdir");
        std::fs::create_dir_all(&state_dir).expect("mkdir");
        init_repo(&repo);

        let source = source_plugin();
        std::fs::copy(
            source.join("hooks/hooks.json"),
            root.join("hooks/hooks.json"),
        )
        .expect("hooks.json must be committed for an install to get it");
        std::fs::copy(source.join(".mcp.json"), root.join(".mcp.json"))
            .expect(".mcp.json must be committed for an install to get it");
        std::fs::copy(MAGENT, root.join("bin/magent")).expect("stage the binary");

        Self {
            _dir: dir,
            root,
            state_dir,
            repo,
        }
    }

    fn manifest(&self, name: &str) -> Value {
        let text = std::fs::read_to_string(self.root.join(name)).expect("read manifest");
        serde_json::from_str(&text).expect("parse manifest")
    }

    /// The command string this event is declared with.
    fn hook_command(&self, event: &str) -> String {
        self.manifest("hooks/hooks.json")["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("{event} declares no command"))
            .to_owned()
    }

    /// Runs a declared hook exactly as the harness would: through a shell, with
    /// `CLAUDE_PLUGIN_ROOT` set and the event JSON on stdin.
    fn fire(&self, event: &str, input: &Value) -> String {
        let command = self.hook_command(event);

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("CLAUDE_PLUGIN_ROOT", &self.root)
            .env("MAGENT_STATE_DIR", &self.state_dir)
            // Deliberately emptied: the whole point is that the manifest must
            // not depend on finding `magent` on PATH.
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the hook");

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
            "{event} failed running {command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn event(&self, session: &str, extra: &Value) -> Value {
        let mut input = json!({
            "session_id": session,
            "cwd": self.repo,
            "transcript_path": self.state_dir.join("transcript.jsonl"),
        });
        if let Some(fields) = extra.as_object() {
            for (key, value) in fields {
                input[key] = value.clone();
            }
        }
        input
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
    std::fs::write(root.join("go.mod"), "module acme/service\n\ngo 1.24.3\n").expect("write");
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

// --- the contract that broke twice -----------------------------------------

/// The exact failure a live install produced: `Executable not found in $PATH`.
/// PATH is stripped here so a manifest that regressed to a bare `magent` cannot
/// pass by accident on a developer's machine.
#[test]
fn every_declared_hook_runs_without_magent_on_path() {
    let installed = Installed::new();

    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "PreCompact",
        "SessionEnd",
    ] {
        installed.fire(event, &installed.event("s1", &json!({})));
    }
}

/// A manifest that is not committed leaves an install with no MCP server, which
/// looks exactly like a working install until a tool is called.
#[test]
fn the_declared_mcp_server_starts_and_speaks_the_protocol() {
    use std::io::{BufRead, BufReader};

    let installed = Installed::new();
    let mcp = installed.manifest(".mcp.json");
    let command = mcp["magent"]["command"].as_str().expect("a command");
    let args: Vec<String> = mcp["magent"]["args"]
        .as_array()
        .expect("args")
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_owned())
        .collect();

    let expanded = command.replace("${CLAUDE_PLUGIN_ROOT}", &installed.root.to_string_lossy());

    // Killed below; wait_with_output would block on a server that only exits
    // when its stdin closes.
    let mut child = Command::new(&expanded)
        .args(&args)
        .arg("--state-dir")
        .arg(&installed.state_dir)
        .current_dir(&installed.repo)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{expanded} could not be started: {error}"));

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "installed-plugin-test", "version": "0" }
        }
    });

    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(format!("{initialize}\n").as_bytes())
        .expect("write");
    stdin.flush().expect("flush");

    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read a response");
    child.kill().expect("kill");
    child.wait().expect("reap");

    let response: Value = serde_json::from_str(&line)
        .unwrap_or_else(|error| panic!("not a protocol frame ({error}): {line:?}"));
    assert_eq!(response["result"]["serverInfo"]["name"], "magent");
    assert!(
        response["result"]["instructions"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty()),
        "an installed server must ship its instructions"
    );
}

// --- a session, start to finish, through the installed plugin ---------------

/// The product's promise, exercised only through what an install provides.
#[test]
fn an_installed_plugin_carries_a_task_across_a_compaction() {
    let installed = Installed::new();

    assert!(
        installed
            .fire(
                "SessionStart",
                &installed.event("s1", &json!({"startup_reason": "startup"}))
            )
            .trim()
            .is_empty(),
        "an empty workspace must say nothing"
    );

    installed.fire(
        "UserPromptSubmit",
        &installed.event(
            "s1",
            &json!({"prompt": "make the retry budget configurable"}),
        ),
    );

    let edited = installed.repo.join("src/client.rs");
    std::fs::create_dir_all(edited.parent().expect("parent")).expect("mkdir");
    std::fs::write(&edited, "pub const RETRIES: u32 = 3;\n").expect("write");
    installed.fire(
        "PostToolUse",
        &installed.event(
            "s1",
            &json!({"tool_name": "Edit", "tool_input": {"file_path": edited}}),
        ),
    );

    installed.fire(
        "PreCompact",
        &installed.event("s1", &json!({"compaction_reason": "auto"})),
    );

    let restored = installed.fire(
        "SessionStart",
        &installed.event("s1", &json!({"startup_reason": "compact"})),
    );

    assert!(
        restored.contains("make the retry budget configurable"),
        "the task did not survive:\n{restored}"
    );
    assert!(
        restored.contains("src/client.rs"),
        "the observed edit did not survive:\n{restored}"
    );
}

/// Toolchain detection runs from `SessionStart`, so an installed plugin should
/// know what the repository declares without anyone asking.
#[test]
fn an_installed_plugin_learns_the_repository_toolchain() {
    let installed = Installed::new();

    installed.fire(
        "UserPromptSubmit",
        &installed.event("s1", &json!({"prompt": "start work"})),
    );
    installed.fire(
        "SessionStart",
        &installed.event("s2", &json!({"startup_reason": "startup"})),
    );

    let store = magent_store::Store::open(&installed.state_dir.join("magent.db")).expect("open");
    let found = store
        .search(&magent_store::FactQuery {
            text: Some("go module version".into()),
            namespaces: vec![
                installed
                    .repo
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
            ],
            ..magent_store::FactQuery::default()
        })
        .expect("search");

    assert!(
        found.iter().any(|fact| fact.title.contains("1.24.3")),
        "the declared Go version was not learned: {:?}",
        found.iter().map(|fact| &fact.title).collect::<Vec<_>>()
    );
}

// --- degradation ------------------------------------------------------------

/// Magent being broken must cost the user nothing. A hook that fails or writes
/// to stdout would surface as a session error or as garbage in the
/// conversation.
#[test]
fn a_broken_install_still_lets_every_hook_exit_cleanly() {
    let installed = Installed::new();
    std::fs::write(
        installed.state_dir.join("magent.db"),
        b"this is not a sqlite file",
    )
    .expect("corrupt the store");

    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreCompact",
        "SessionEnd",
    ] {
        let output = installed.fire(event, &installed.event("s1", &json!({})));
        assert!(
            output.is_empty(),
            "{event} wrote to stdout while degraded: {output:?}"
        );
    }
}
