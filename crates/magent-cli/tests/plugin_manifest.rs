//! The plugin manifest's own contract.
//!
//! These assertions exist because a live install failed in a way no other test
//! could have caught. The plugin's `bin/` directory is added to the **Bash
//! tool's** PATH and nothing else: hooks and MCP servers are launched without
//! it. A bare `magent` in either manifest therefore resolves during development,
//! where the binary happens to be on PATH, and fails on a clean machine with
//! `Executable not found in $PATH`.
//!
//! Every command must go through `${CLAUDE_PLUGIN_ROOT}`, which Claude Code
//! expands to wherever it copied the plugin.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Set by Claude Code to the installed plugin's directory.
const PLUGIN_ROOT: &str = "${CLAUDE_PLUGIN_ROOT}";

fn plugin_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/magent-cli.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugin")
        .canonicalize()
        .expect("the plugin directory should sit at the repository root")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()))
}

/// Every string under `value` at the key `command`.
fn commands(value: &Value) -> Vec<String> {
    let mut found = Vec::new();

    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                if key == "command"
                    && let Some(command) = child.as_str()
                {
                    found.push(command.to_owned());
                }
                found.extend(commands(child));
            }
        }
        Value::Array(items) => {
            for item in items {
                found.extend(commands(item));
            }
        }
        _ => {}
    }

    found
}

// --- what the live install caught ------------------------------------------

#[test]
fn every_hook_command_resolves_through_the_plugin_root() {
    let hooks = read_json(&plugin_dir().join("hooks/hooks.json"));
    let commands = commands(&hooks);

    assert!(!commands.is_empty(), "no hooks are declared");

    for command in &commands {
        assert!(
            command.contains(PLUGIN_ROOT),
            "hook command {command:?} relies on PATH; the plugin's bin/ reaches \
             the Bash tool only, so this fails on a clean install"
        );
    }
}

#[test]
fn the_mcp_server_command_resolves_through_the_plugin_root() {
    let mcp = read_json(&plugin_dir().join(".mcp.json"));
    let commands = commands(&mcp);

    assert!(!commands.is_empty(), "no MCP server is declared");

    for command in &commands {
        assert!(
            command.contains(PLUGIN_ROOT),
            "MCP command {command:?} relies on PATH, which is not provided to \
             servers the plugin launches"
        );
    }
}

/// The path the manifests point at has to be the one the build produces.
#[test]
fn the_referenced_binary_path_is_the_one_the_install_script_writes() {
    let hooks = read_json(&plugin_dir().join("hooks/hooks.json"));
    let mcp = read_json(&plugin_dir().join(".mcp.json"));

    let install = std::fs::read_to_string(plugin_dir().join("../scripts/install.sh"))
        .expect("install.sh should sit beside the plugin");
    assert!(
        install.contains("plugin/bin/magent"),
        "install.sh no longer writes plugin/bin/magent"
    );

    for command in commands(&hooks).into_iter().chain(commands(&mcp)) {
        assert!(
            command.contains("bin/magent"),
            "{command:?} does not point at the built binary"
        );
    }
}

// --- events -----------------------------------------------------------------

/// Every event the manifest subscribes to must be one the binary handles.
/// A typo here is silent: Claude Code fires the hook, the binary rejects the
/// name, and nothing is ever recorded.
#[test]
fn every_declared_event_is_one_the_binary_handles() {
    let hooks = read_json(&plugin_dir().join("hooks/hooks.json"));

    for command in commands(&hooks) {
        let event = command
            .rsplit_once(' ')
            .map(|(_, event)| event.to_owned())
            .expect("a hook command names its event");

        assert!(
            magent_cli::hook::Event::parse(&event).is_some(),
            "the manifest subscribes to {event:?}, which the binary does not handle"
        );
    }
}

/// `PreCompact` is what makes the checkpoint exist before context is thrown
/// away. Dropping it from the manifest would leave every other test passing
/// while the product's central promise quietly stopped working.
#[test]
fn the_events_the_design_depends_on_are_all_subscribed() {
    let hooks = read_json(&plugin_dir().join("hooks/hooks.json"));
    let declared = hooks
        .get("hooks")
        .and_then(Value::as_object)
        .expect("a hooks object");

    for required in [
        "SessionStart",
        "UserPromptSubmit",
        "PreCompact",
        "PostToolUse",
    ] {
        assert!(
            declared.contains_key(required),
            "{required} is not subscribed to"
        );
    }
}
