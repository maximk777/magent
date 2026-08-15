//! Hook handlers.
//!
//! Hooks fire whether or not the model cooperates, which is what makes run
//! identity, the file ledger and the pre-compaction checkpoint guaranteed
//! rather than best-effort.
//!
//! The price of that reach is strict manners. Every handler here:
//! never blocks, never fails the session, and writes to stdout only when it has
//! something worth injecting. Enforcement lives in `main`, which converts any
//! error into a stderr note and exit 0.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use magent_core::{FileLedgerEntry, HarnessKind};
use magent_store::Store;
use serde_json::Value;

use crate::packet;

/// Ledger entries folded into a deterministic checkpoint.
const LEDGER_LIMIT: usize = 200;

/// Job kind for turning a transcript into reasoning.
pub const ENRICH_JOB: &str = "enrich_checkpoint";
/// Job kind for turning a finished session into durable facts.
pub const DISTILL_JOB: &str = "distill_session";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    PreCompact,
    SessionEnd,
    SubagentStop,
    Stop,
}

impl Event {
    /// Parses the CLI subcommand name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "session-start" => Self::SessionStart,
            "user-prompt-submit" => Self::UserPromptSubmit,
            "post-tool-use" => Self::PostToolUse,
            "pre-compact" => Self::PreCompact,
            "session-end" => Self::SessionEnd,
            "subagent-stop" => Self::SubagentStop,
            "stop" => Self::Stop,
            _ => return None,
        })
    }
}

/// Runs a hook, returning whatever should go to stdout.
///
/// # Errors
///
/// Returns any underlying failure so the caller can log it and still exit 0.
pub fn handle(event: Event, input: &Value, state_dir: &Path) -> anyhow::Result<String> {
    let Some(session) = string_field(input, "session_id") else {
        // Without a session id nothing can be attributed. Not an error: some
        // events legitimately arrive without one.
        return Ok(String::new());
    };

    match event {
        // Recording a subagent's findings and nudging on a stale checkpoint both
        // belong to later slices. The events are wired now so the plugin
        // manifest stays stable and the no-op path is covered by tests.
        Event::SubagentStop | Event::Stop => return Ok(String::new()),
        _ => {}
    }

    let store = Store::open(&crate::paths::database_path(state_dir))?;
    let cwd = cwd_of(input);

    match event {
        Event::SessionStart => session_start(&store, &session, &cwd),
        Event::UserPromptSubmit => user_prompt_submit(&store, &session, &cwd, input),
        Event::PostToolUse => post_tool_use(&store, &session, input),
        Event::PreCompact => pre_compact(&store, &session, &cwd, input),
        Event::SessionEnd => session_end(&store, &session, input),
        Event::SubagentStop | Event::Stop => Ok(String::new()),
    }
}

/// Restores context after a compaction, a `/clear`, a resume or a fresh start.
///
/// Silent when the workspace has no open run: announcing Magent where nothing
/// is in flight would spend context on every session for no benefit.
fn session_start(store: &Store, session: &str, cwd: &Path) -> anyhow::Result<String> {
    let Some(binding) = store.attach_to_open_run(session, cwd, HarnessKind::ClaudeCode)? else {
        return Ok(String::new());
    };

    let snapshot = store.snapshot(binding.run_id)?;
    let ledger = store.ledger(binding.run_id, LEDGER_LIMIT)?;
    let git = magent_store::git_state(cwd);
    let root = magent_store::repository_root(cwd);

    Ok(packet::render(
        &snapshot,
        &ledger,
        git.as_ref(),
        root.as_deref(),
        Utc::now(),
    )
    .unwrap_or_default())
}

/// Opens a run on the first prompt of a session that has none.
///
/// This is the hook layer's whole point: a task exists because the user asked
/// for work, not because the model remembered to announce it.
fn user_prompt_submit(
    store: &Store,
    session: &str,
    cwd: &Path,
    input: &Value,
) -> anyhow::Result<String> {
    let prompt = prompt_text(input).unwrap_or_else(|| "(untitled session)".to_owned());
    store.bind_session(session, cwd, &prompt, HarnessKind::ClaudeCode)?;
    Ok(String::new())
}

/// Appends one observed mutation to the ledger. Deliberately silent: the ledger
/// exists to be read at restore time, not narrated as it fills.
fn post_tool_use(store: &Store, session: &str, input: &Value) -> anyhow::Result<String> {
    let Some(path) = edited_path(input) else {
        return Ok(String::new());
    };

    store.append_ledger_for_external_session(
        session,
        &FileLedgerEntry {
            path,
            tool: string_field(input, "tool_name").unwrap_or_else(|| "unknown".into()),
            observed_at: Utc::now(),
        },
    )?;

    Ok(String::new())
}

/// Writes a checkpoint from observation alone, then queues the reasoning.
///
/// The split exists because `PreCompact` cannot inject context and cannot wait:
/// compaction may complete before any model call would. What is provably true
/// is written now, synchronously, so `SessionStart` always finds something.
fn pre_compact(store: &Store, session: &str, cwd: &Path, input: &Value) -> anyhow::Result<String> {
    let Some(binding) = store.binding_for_external_session(session)? else {
        return Ok(String::new());
    };

    let ledger = store.ledger(binding.run_id, LEDGER_LIMIT)?;
    let changed_files: Vec<String> = ledger
        .iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect();

    let git = magent_store::git_state(cwd);
    let summary = observed_summary(changed_files.len(), git.as_ref());

    store.record_observed_checkpoint(binding, changed_files, Vec::new(), summary)?;

    store.enqueue_job(
        ENRICH_JOB,
        &binding.run_id.to_string(),
        &serde_json::json!({
            "run_id": binding.run_id,
            "session_id": binding.session_id,
            "transcript_path": string_field(input, "transcript_path"),
        })
        .to_string(),
    )?;

    // PreCompact can only allow or deny; anything written here is discarded.
    Ok(String::new())
}

fn session_end(store: &Store, session: &str, input: &Value) -> anyhow::Result<String> {
    if let Some(binding) = store.binding_for_external_session(session)? {
        store.enqueue_job(
            DISTILL_JOB,
            &binding.session_id.to_string(),
            &serde_json::json!({
                "run_id": binding.run_id,
                "session_id": binding.session_id,
                "transcript_path": string_field(input, "transcript_path"),
            })
            .to_string(),
        )?;
    }

    // The run stays open: closing an editor is not finishing a task.
    store.close_external_session(session)?;
    Ok(String::new())
}

fn observed_summary(file_count: usize, git: Option<&magent_core::GitState>) -> String {
    let mut summary = format!("Context compacted; {file_count} file(s) touched this run");

    if let Some(state) = git {
        let branch = state.branch.as_deref().unwrap_or("detached HEAD");
        let _ = write!(
            summary,
            "; on {branch} with {} uncommitted",
            state.dirty_files
        );
    }

    summary.push_str(". Reasoning pending enrichment.");
    summary
}

// --- input access ----------------------------------------------------------
//
// Hook payloads are read defensively. A field that moves or gains a variant
// must degrade to "not present" rather than break the session.

fn string_field(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

fn cwd_of(input: &Value) -> PathBuf {
    string_field(input, "cwd").map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    )
}

/// The prompt text, under either of the names this field has carried.
fn prompt_text(input: &Value) -> Option<String> {
    string_field(input, "prompt")
        .or_else(|| string_field(input, "user_input"))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn edited_path(input: &Value) -> Option<PathBuf> {
    let raw = input.get("tool_input")?.get("file_path")?.as_str()?;
    Some(canonicalize_best_effort(Path::new(raw)))
}

/// Resolves symlinks so ledger paths are comparable with the repository root.
///
/// Without this the restoration packet cannot shorten them: on macOS a session
/// started under `/var/folders/...` records paths there while the repository
/// root resolves to `/private/var/folders/...`, so the prefixes never match and
/// every file is printed in full, on every session.
///
/// Falls back to canonicalising the parent for a path that does not exist yet,
/// and to the original path when even that fails: a ledger entry is worth
/// keeping in a less tidy form.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }

    // Walk up to the deepest ancestor that exists, then re-attach the rest.
    let mut suffix = Vec::new();
    let mut current = path;
    while let Some(parent) = current.parent() {
        let Some(name) = current.file_name() else {
            break;
        };
        suffix.push(name);

        if let Ok(resolved) = std::fs::canonicalize(parent) {
            return suffix
                .iter()
                .rev()
                .fold(resolved, |accumulated, part| accumulated.join(part));
        }
        current = parent;
    }

    path.to_path_buf()
}
