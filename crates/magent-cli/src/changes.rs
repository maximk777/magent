//! What is in flight, printed for a reader rather than a parser.
//!
//! The three spec-driven skills open by running this in a `!` line, so its
//! output lands in front of the model before the turn starts. That placement
//! decides the shape of everything below. Silence is not an option — in that
//! position an empty answer is indistinguishable from a broken install, and the
//! reader's next move would be to debug Magent instead of doing the work. And
//! nothing here is a data format: a skill injects these lines to be read, so
//! they are laid out for eyes, and `magent_changes` stays the way to get the
//! same facts as structure.

use std::{fmt::Write as _, path::Path};

use magent_core::{ChangeId, ChangeStatus};
use magent_store::{ChangeDetail, ChangeSummary, FactContext, Store};

/// Writes the report, returning false when the command could not answer.
///
/// "Nothing is open" is an answer and returns true. The failures are the ones
/// where the reader would otherwise believe an empty list: a store that will
/// not open, a directory belonging to no workspace, a named change that is not
/// here.
///
/// # Errors
/// Never. Failures are part of the report.
pub fn report(state_dir: &Path, here: &Path, reference: Option<&str>, out: &mut String) -> bool {
    let database = crate::paths::database_path(state_dir);
    let store = match Store::open(&database) {
        Ok(store) => store,
        Err(error) => {
            let _ = writeln!(out, "cannot open {}: {error}", database.display());
            return false;
        }
    };

    let context = match context_for(&store, here) {
        Ok(context) => context,
        Err(message) => {
            let _ = writeln!(out, "{message}");
            return false;
        }
    };

    match reference {
        Some(reference) => report_one(&store, &context, reference, out),
        None => report_open(&store, &context, out),
    }
}

/// The workspace and namespace this directory reads under.
///
/// Resolved exactly as the MCP server resolves it, from the repository root
/// rather than the working directory: a command run from a subdirectory has to
/// see the same changes as one run from the top, or the skills would print
/// different answers depending on where the session happened to start.
fn context_for(store: &Store, here: &Path) -> Result<FactContext, String> {
    let root = magent_store::repository_root(here).unwrap_or_else(|| here.to_path_buf());

    let workspace = store
        .resolve_workspace_for(&root)
        .map_err(|error| format!("cannot resolve a workspace for {}: {error}", root.display()))?;

    Ok(FactContext {
        workspace_id: Some(workspace.workspace_id),
        namespace: namespace_of(&root),
        ..FactContext::default()
    })
}

fn namespace_of(root: &Path) -> Option<String> {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn report_open(store: &Store, context: &FactContext, out: &mut String) -> bool {
    let changes = match store.open_changes(context) {
        Ok(changes) => changes,
        Err(error) => {
            let _ = writeln!(out, "cannot read the changes here: {error}");
            return false;
        }
    };

    if changes.is_empty() {
        // Phrased as a finding, not as an error: proposing something is the
        // ordinary next move from here, and the line says so rather than
        // leaving the reader to wonder whether the lookup failed.
        let _ = writeln!(
            out,
            "no change is open here — magent_propose starts one, and the whole \
             list is read back with magent_changes"
        );
        return true;
    }

    let _ = writeln!(out, "open changes");
    let width = changes
        .iter()
        .map(|change| change.slug.len())
        .max()
        .unwrap_or_default();

    for change in &changes {
        let _ = writeln!(out, "\n  {:<width$}  {}", change.slug, summary_of(change));
        let _ = writeln!(out, "  {:<width$}  {}", "", change.title);
    }

    true
}

/// The one line that says where a change got to: its status, how much of the
/// plan is closed, and how many requirements it has filed.
///
/// The closed count is the reason this exists — a status of `planned` is true
/// of a change with no task closed and of one with every task but the last,
/// and those are different situations for whoever reads it next.
fn summary_of(change: &ChangeSummary) -> String {
    format!(
        "{}  tasks {}/{}  deltas {}",
        status_name(change.status),
        change.tasks_closed,
        change.task_count,
        change.delta_count,
    )
}

fn report_one(store: &Store, context: &FactContext, reference: &str, out: &mut String) -> bool {
    let Some(change) = resolve(store, context, reference, out) else {
        return false;
    };

    let detail = match store.change_detail(change, context) {
        Ok(Some(detail)) => detail,
        Ok(None) => {
            let _ = writeln!(out, "no change here is called {reference}");
            return false;
        }
        Err(error) => {
            let _ = writeln!(out, "cannot read {reference}: {error}");
            return false;
        }
    };

    write_detail(&detail, out);
    true
}

fn write_detail(detail: &ChangeDetail, out: &mut String) {
    let _ = writeln!(out, "{}  {}", detail.slug, status_name(detail.status));
    let _ = writeln!(out, "  {}", detail.title);

    // Why first, and in the proposal's own words. Someone picking a change up
    // needs the reason before the list of moves: a task list read without it is
    // a set of instructions with no way to judge whether they still apply.
    let _ = writeln!(out, "\nwhy");
    for line in detail.why.lines() {
        let _ = writeln!(out, "  {line}");
    }

    if !detail.deltas.is_empty() {
        let _ = writeln!(out, "\nrequirements  {}", detail.deltas.len());
        for delta in &detail.deltas {
            let _ = writeln!(
                out,
                "  {}  {}  {}",
                delta_op_name(delta.op),
                delta.capability_path,
                delta.name,
            );
        }
    }

    if detail.tasks.is_empty() {
        let _ = writeln!(out, "\ntasks  none yet — magent_plan writes them");
        return;
    }

    let _ = writeln!(
        out,
        "\ntasks  {}/{} closed",
        detail.tasks_closed, detail.task_count
    );

    let number_width = detail
        .tasks
        .iter()
        .map(|task| task.number.len())
        .max()
        .unwrap_or_default();
    let status_width = detail
        .tasks
        .iter()
        .map(|task| task.status.len())
        .max()
        .unwrap_or_default();

    // Ordered here rather than taken as it comes. The store sorts task numbers
    // as text, which reads 1, 10, 11, 2 — fine for a caller that looks a number
    // up, wrong for the one thing this command is for, which is a person
    // reading the plan in order. Sorting for display does not fix the store's
    // own order, and `magent_changes` still hands its callers the text one.
    let mut tasks: Vec<_> = detail.tasks.iter().collect();
    tasks.sort_by(|left, right| order_of(&left.number).cmp(&order_of(&right.number)));

    for task in tasks {
        let _ = writeln!(
            out,
            "  {:>number_width$}  {:<status_width$}  {}",
            task.number, task.status, task.title,
        );
    }
}

/// Turns a reference into an identifier, the way the MCP tool does: a UUID is
/// passed through, anything else is a slug looked up among what is open here.
///
/// A slug that matches nothing is refused with the open slugs, because the
/// reader who mistyped one is standing in front of the list they needed.
fn resolve(
    store: &Store,
    context: &FactContext,
    reference: &str,
    out: &mut String,
) -> Option<ChangeId> {
    if let Ok(change) = reference.parse::<ChangeId>() {
        return Some(change);
    }

    let open = match store.open_changes(context) {
        Ok(open) => open,
        Err(error) => {
            let _ = writeln!(out, "cannot read the changes here: {error}");
            return None;
        }
    };

    if let Some(found) = open.iter().find(|change| change.slug == reference) {
        return Some(found.id);
    }

    if open.is_empty() {
        let _ = writeln!(
            out,
            "no change here is called {reference}, and nothing is open here at all"
        );
    } else {
        let slugs: Vec<&str> = open.iter().map(|change| change.slug.as_str()).collect();
        let _ = writeln!(
            out,
            "no change here is called {reference}. Open here: {}",
            slugs.join(", ")
        );
    }

    None
}

/// What a task number sorts by: its leading integer first, then the whole
/// string. A plan numbers its tasks 1..16, and `10` belongs after `9`.
///
/// The string is kept as the second key rather than dropped, so numbers a plan
/// might carry that are not plain integers — `4a`, or a number this parses no
/// part of — still order against each other predictably instead of colliding.
fn order_of(number: &str) -> (u64, &str) {
    let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(u64::MAX), number)
}

/// A status as the store spells it, taken through serde rather than a match
/// written here: a variant added to the enum then reaches this as its own name
/// instead of as something this file invented for it.
fn status_name(status: ChangeStatus) -> String {
    match serde_json::to_value(status) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}

fn delta_op_name(op: magent_core::DeltaOp) -> String {
    match serde_json::to_value(op) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}
