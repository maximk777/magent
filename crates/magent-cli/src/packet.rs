//! The context restoration packet.
//!
//! Written to stdout by `SessionStart`, where Claude Code injects it into the
//! conversation. It has to earn its tokens: dense enough to resume work, small
//! enough that paying for it every session is obviously worth it. Detail stays
//! in the store, one `magent_recall` away.
//!
//! English, not Russian: this is tool surface consumed by the model, the same
//! as MCP server instructions.

use std::fmt::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use magent_core::{CheckpointOrigin, CheckpointSnapshot, FileLedgerEntry, GitState, RunSnapshot};

/// Files listed by name before the rest are collapsed into a count.
const FILE_LIMIT: usize = 8;

/// Renders the packet, or `None` when there is nothing worth saying.
///
/// `root` is the repository root. Paths beneath it are rendered relative:
/// absolute paths are mostly one repeated prefix, and this text is paid for in
/// context on every session.
#[must_use]
pub fn render(
    run: &RunSnapshot,
    ledger: &[FileLedgerEntry],
    git: Option<&GitState>,
    root: Option<&Path>,
    now: DateTime<Utc>,
) -> Option<String> {
    if run.task.trim().is_empty() && run.latest_checkpoint.is_none() {
        return None;
    }

    let mut out = String::from("## Magent: context restored\n");

    let _ = writeln!(out, "run {} · stage {}", run.run_id, stage_name(run.stage));
    let _ = writeln!(out, "Task: {}", run.task.trim());

    if let Some(checkpoint) = run.latest_checkpoint.as_ref() {
        let _ = writeln!(
            out,
            "Checkpoint: {} ({})",
            age(now, checkpoint.created_at),
            origin_name(checkpoint.origin)
        );
        push_checkpoint(&mut out, checkpoint);
    } else {
        out.push_str("Checkpoint: none yet\n");
    }

    push_files(&mut out, run.latest_checkpoint.as_ref(), ledger, root);

    if let Some(state) = git {
        let _ = writeln!(out, "Git: {}", describe_git(state));
    }

    out.push_str("Detail: magent_search, magent_recall\n");
    Some(out)
}

fn push_checkpoint(out: &mut String, checkpoint: &CheckpointSnapshot) {
    push_list(out, "Next", &checkpoint.next_steps);
    push_list(out, "Done", &checkpoint.completed_steps);
    push_list(out, "Decisions", &checkpoint.decisions);
    // Carried so a resumed session does not re-propose what was already turned
    // down — the most expensive kind of lost context.
    push_list(out, "Rejected", &checkpoint.rejected);
    push_list(out, "Verified", &checkpoint.verification);
    push_list(out, "Risks", &checkpoint.risks);

    let summary = checkpoint.handoff_summary.trim();
    if !summary.is_empty() {
        let _ = writeln!(out, "Summary: {summary}");
    }
}

/// Prefers the checkpoint's own file list and falls back to the observed
/// ledger, which is present even when no model ever described the work.
fn push_files(
    out: &mut String,
    checkpoint: Option<&CheckpointSnapshot>,
    ledger: &[FileLedgerEntry],
    root: Option<&Path>,
) {
    let from_checkpoint = checkpoint
        .map(|c| c.changed_files.clone())
        .unwrap_or_default();

    let files: Vec<String> = if from_checkpoint.is_empty() {
        ledger
            .iter()
            .map(|entry| entry.path.to_string_lossy().into_owned())
            .collect()
    } else {
        from_checkpoint
    };

    if files.is_empty() {
        return;
    }

    let shown: Vec<String> = files
        .iter()
        .take(FILE_LIMIT)
        .map(|file| relative_to(file, root))
        .collect();
    let remainder = files.len().saturating_sub(shown.len());

    let mut line = format!("Files: {}", shown.join(", "));
    if remainder > 0 {
        let _ = write!(line, " (+{remainder} more)");
    }
    line.push('\n');
    out.push_str(&line);
}

/// Strips the repository prefix when the file is inside it.
///
/// A path outside the repository keeps its absolute form: shortening it would
/// make it ambiguous, and those are rare enough that the tokens do not matter.
fn relative_to(file: &str, root: Option<&Path>) -> String {
    root.and_then(|root| Path::new(file).strip_prefix(root).ok())
        .map_or_else(
            || file.to_owned(),
            |rest| rest.to_string_lossy().into_owned(),
        )
}

fn push_list(out: &mut String, label: &str, items: &[String]) {
    let joined = items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join("; ");

    if !joined.is_empty() {
        let _ = writeln!(out, "{label}: {joined}");
    }
}

fn describe_git(state: &GitState) -> String {
    let branch = state.branch.as_deref().unwrap_or("detached");
    let short = state.sha.as_ref().map_or_else(String::new, |sha| {
        format!(" @ {}", &sha[..sha.len().min(7)])
    });

    if state.dirty_files == 0 {
        format!("{branch}{short}, clean")
    } else {
        format!("{branch}{short}, {} uncommitted", state.dirty_files)
    }
}

/// Coarse on purpose: "how stale is this" is the only question being answered,
/// and an exact timestamp would cost tokens to say the same thing.
fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let minutes = (now - then).num_minutes().max(0);

    match minutes {
        0 => "just now".into(),
        1..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

fn stage_name(stage: magent_core::WorkflowStage) -> &'static str {
    use magent_core::WorkflowStage::{
        Completed, Discovering, Executing, Planning, Reviewing, Verifying,
    };
    match stage {
        Discovering => "discovering",
        Planning => "planning",
        Executing => "executing",
        Verifying => "verifying",
        Reviewing => "reviewing",
        Completed => "completed",
    }
}

/// Named in the packet so the model knows whether the reasoning behind this
/// checkpoint is present or still being distilled.
fn origin_name(origin: CheckpointOrigin) -> &'static str {
    match origin {
        CheckpointOrigin::Deterministic => "deterministic",
        CheckpointOrigin::Enriched => "enriched",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use magent_core::{RunId, RunStatus, SessionId, WorkflowStage, WorkspaceId};

    use super::{FILE_LIMIT, render};

    /// The packet is injected on every session, so its size is a running cost.
    /// A real ledger is dominated by one repeated directory prefix.
    const SIZE_BUDGET: usize = 1024;

    fn ledger(root: &str, count: usize) -> Vec<magent_core::FileLedgerEntry> {
        (0..count)
            .map(|index| magent_core::FileLedgerEntry {
                path: PathBuf::from(format!("{root}/crates/service/src/handler_{index}.rs")),
                tool: "Edit".into(),
                observed_at: chrono::Utc::now(),
            })
            .collect()
    }

    fn run(task: &str) -> magent_core::RunSnapshot {
        magent_core::RunSnapshot {
            run_id: RunId::new(),
            workspace_id: WorkspaceId::new(),
            task: task.into(),
            status: RunStatus::Open,
            stage: WorkflowStage::Executing,
            latest_checkpoint: None,
        }
    }

    #[test]
    fn files_inside_the_repository_are_rendered_relative() {
        let root = "/Users/someone/programming/acme/service";
        let rendered = render(
            &run("fix the timeout"),
            &ledger(root, 3),
            None,
            Some(std::path::Path::new(root)),
            chrono::Utc::now(),
        )
        .expect("a packet");

        assert!(
            rendered.contains("crates/service/src/handler_0.rs"),
            "{rendered}"
        );
        assert!(
            !rendered.contains(root),
            "the repository prefix must not be repeated:\n{rendered}"
        );
    }

    /// Shortening a path that lies outside the repository would make it
    /// ambiguous, and those are rare enough that the tokens do not matter.
    #[test]
    fn files_outside_the_repository_keep_their_absolute_path() {
        let mut entries = ledger("/Users/someone/programming/acme/service", 1);
        entries.push(magent_core::FileLedgerEntry {
            path: PathBuf::from("/etc/hosts"),
            tool: "Write".into(),
            observed_at: chrono::Utc::now(),
        });

        let rendered = render(
            &run("fix the timeout"),
            &entries,
            None,
            Some(std::path::Path::new(
                "/Users/someone/programming/acme/service",
            )),
            chrono::Utc::now(),
        )
        .expect("a packet");

        assert!(rendered.contains("/etc/hosts"), "{rendered}");
    }

    #[test]
    fn a_long_session_still_produces_a_small_packet() {
        let root = "/Users/someone/programming/acme/service";
        let rendered = render(
            &run("a wide refactor across the service"),
            &ledger(root, 200),
            None,
            Some(std::path::Path::new(root)),
            chrono::Utc::now(),
        )
        .expect("a packet");

        assert!(
            rendered.len() <= SIZE_BUDGET,
            "packet is {} bytes, budget is {SIZE_BUDGET}:\n{rendered}",
            rendered.len()
        );
        assert!(
            rendered.contains(&format!("(+{} more)", 200 - FILE_LIMIT)),
            "the files that did not fit must still be counted:\n{rendered}"
        );
    }

    /// A session with no checkpoint and no task has nothing worth injecting.
    #[test]
    fn an_empty_run_renders_nothing() {
        let mut empty = run("");
        empty.latest_checkpoint = None;
        let _ = SessionId::new();

        assert!(render(&empty, &[], None, None, chrono::Utc::now()).is_none());
    }
}
