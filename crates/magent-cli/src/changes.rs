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
use magent_store::{ChangeDetail, ChangeSummary, DeltaSummary, FactContext, Store};

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

    // None: a person at a terminal holds no task, so every live hold is
    // somebody else's and shows as taken.
    let detail = match store.change_detail(change, context, None) {
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
            write_delta(delta, out);
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

    push_shape_and_ready(out, detail);
    push_journal(out, detail);
}

/// How parallel the plan is, and what may be started right now.
///
/// The conflict line is what makes the ready list usable: a reader seeing three
/// ready tasks and dispatching all three is exactly the collision this exists
/// to prevent. A width of 1 is printed rather than hidden — it says the plan is
/// a chain, which is a real answer and better than an offer that turns out
/// empty after the agents have been briefed.
fn push_shape_and_ready(out: &mut String, detail: &ChangeDetail) {
    let _ = writeln!(
        out,
        "\nshape  width {}, longest chain {}",
        detail.shape.width, detail.shape.longest_chain
    );

    if detail.ready.is_empty() {
        let _ = writeln!(out, "ready  nothing — every task is closed or waiting");
        return;
    }

    let mut ready: Vec<_> = detail.ready.iter().collect();
    ready.sort_by(|left, right| order_of(&left.number).cmp(&order_of(&right.number)));

    let numbers: Vec<&str> = ready.iter().map(|task| task.number.as_str()).collect();
    let _ = writeln!(out, "ready  {}", numbers.join(", "));

    for task in ready {
        if task.conflicts_with.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "       {} shares a file with {}",
            task.number,
            task.conflicts_with.join(", ")
        );
    }
}

/// One requirement delta, in the words a reviewer has to judge.
///
/// The header line — op, capability, name — is enough to look a requirement
/// up, and looking it up is exactly what the `sdd-brainstorm` skill's closing
/// question does not leave room for: it asks a person to approve a
/// specification in the terminal, and a name approved on trust is the model
/// reviewing its own work. So the text comes with it, printed line for line at
/// an indent the way the proposal's `why` is, rather than reflowed — the
/// author's paragraph breaks are part of what they wrote.
///
/// The scenarios carry a `scenario` label, and a blank line above them where
/// there is prose to separate them from, because a requirement's text is a
/// paragraph the terminal wraps back to the left margin — an indent alone
/// stops telling the reader where the prose ended and the list began. `given`
/// is dropped when the scenario has none instead of printed empty, because an
/// empty label reads as a scenario missing its precondition rather than as one
/// that needs none.
fn write_delta(delta: &DeltaSummary, out: &mut String) {
    let _ = writeln!(
        out,
        "\n  {}  {}  {}",
        delta_op_name(delta.op),
        delta.capability_path,
        delta.name,
    );

    let mut prose = false;

    if let Some(text) = &delta.text {
        for line in text.lines() {
            let _ = writeln!(out, "    {line}");
        }
        prose = true;
    }

    if let Some(rename_to) = &delta.rename_to {
        let _ = writeln!(out, "    renamed to  {rename_to}");
        prose = true;
    }

    // A removed requirement has no text at all, so these two are the whole of
    // what a reviewer gets to judge the removal by.
    for (label, value) in [("reason", &delta.reason), ("migration", &delta.migration)] {
        if let Some(value) = value {
            let _ = writeln!(out, "    {label:<9}  {value}");
            prose = true;
        }
    }

    if prose && !delta.scenarios.is_empty() {
        let _ = writeln!(out);
    }

    for scenario in &delta.scenarios {
        let _ = writeln!(out, "    scenario  {}", scenario.name);
        if let Some(given) = &scenario.given {
            let _ = writeln!(out, "      {:<5}  {}", "given", given);
        }
        let _ = writeln!(out, "      {:<5}  {}", "when", scenario.when);
        let _ = writeln!(out, "      {:<5}  {}", "then", scenario.then);
    }
}

/// What the plan has already proved, under the plan itself.
///
/// Beneath the tasks rather than beside them, because the two answer different
/// questions and a reader is usually asking the first: the task list is what is
/// left to do, and the journal is what was done. Silent when there is nothing —
/// a heading with nothing under it is the defect this project has already been
/// caught printing once.
///
/// The output is quoted rather than characterised, and truncated to its last
/// line rather than summarised: what makes a tick checkable is that a reader
/// other than its author can see what the command actually printed, and a
/// terminal that had to scroll a full test run to reach the next task would be
/// paying for that in the one place it cannot afford to.
fn push_journal(out: &mut String, detail: &ChangeDetail) {
    if detail.ticks.is_empty() {
        return;
    }

    let _ = writeln!(
        out,
        "\nproved  {} {}",
        detail.ticks.len(),
        if detail.ticks.len() == 1 {
            "tick"
        } else {
            "ticks"
        }
    );

    for tick in &detail.ticks {
        let orphan = if tick.in_current_plan {
            ""
        } else {
            "  (not in the current plan)"
        };
        let _ = writeln!(
            out,
            "  {}  {}{}",
            tick.number,
            tick.verify_command.trim(),
            orphan
        );

        // The last non-blank line: a verify command's verdict is what it prints
        // last, and the lines above it are the run rather than the result.
        if let Some(verdict) = tick
            .output
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
        {
            let _ = writeln!(out, "        {}", verdict.trim());
        }

        if !tick.expected_output_missing.is_empty() {
            let _ = writeln!(
                out,
                "        missing: {}",
                tick.expected_output_missing.join(", ")
            );
        }
    }
}

/// Turns a reference into an identifier, the way the MCP tool does: a UUID is
/// passed through, anything else is a slug looked up among every change of this
/// workspace carrying it — the archived ones included.
///
/// Archived ones included because archiving is not deletion: `magent_archive`
/// keeps the change with its reasoning, and the reader who comes back for that
/// reasoning has the name and nothing else — the id went with the session that
/// had it. Looking the slug up among what is open would answer them "nothing is
/// open here at all", which is true and not what they asked.
///
/// A slug that matches nothing is still refused with the open slugs, because
/// the reader who mistyped one is standing in front of the list they needed.
fn resolve(
    store: &Store,
    context: &FactContext,
    reference: &str,
    out: &mut String,
) -> Option<ChangeId> {
    if let Ok(change) = reference.parse::<ChangeId>() {
        return Some(change);
    }

    let named = match store.changes_named(context, reference) {
        Ok(named) => named,
        Err(error) => {
            let _ = writeln!(out, "cannot read the changes here: {error}");
            return None;
        }
    };

    match named.as_slice() {
        [] => {
            refuse_unknown(store, context, reference, out);
            None
        }
        // Whatever its status, one change of this name is the one meant.
        [only] => Some(only.id),
        // `changes_named` sorts the live one first, and there is at most one:
        // the slug is unique among changes that are neither archived nor
        // abandoned. So a live change wins over the finished ones sharing its
        // name, which is what someone naming a slug while working means.
        [first, ..] if first.status.is_open() => Some(first.id),
        finished => {
            refuse_ambiguous(reference, finished, out);
            None
        }
    }
}

/// The two messages for a slug nothing here answers to: one for a profile with
/// nothing in it, which is usually a fresh install rather than a typo, and one
/// that hands back the slugs there are to choose from.
fn refuse_unknown(store: &Store, context: &FactContext, reference: &str, out: &mut String) {
    let open = match store.open_changes(context) {
        Ok(open) => open,
        Err(error) => {
            let _ = writeln!(out, "cannot read the changes here: {error}");
            return;
        }
    };

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
}

/// Several finished changes of one name, and no answer to which was meant.
///
/// A slug is free again once its change is archived, so a name can belong to
/// several over time. This refuses to choose rather than taking the most
/// recent, because the most recent is wrong exactly when the question matters —
/// when work of the same name was done twice, and the reader is after the
/// earlier one they only half remember. Refusing silently would be worse still:
/// what gets them past this is the ids, and the date each change reached its
/// end is what tells them which id is which.
fn refuse_ambiguous(reference: &str, candidates: &[ChangeSummary], out: &mut String) {
    let _ = writeln!(
        out,
        "{} changes here are called {reference} and none of them is open. \
         Name the one you mean by its id:",
        candidates.len()
    );

    let statuses: Vec<String> = candidates
        .iter()
        .map(|change| status_name(change.status))
        .collect();
    let width = statuses.iter().map(String::len).max().unwrap_or_default();

    for (change, status) in candidates.iter().zip(&statuses) {
        // `updated_at` rather than a column of its own: nothing touches a
        // change after it is archived, so its last touch is when it ended.
        let _ = writeln!(
            out,
            "  {}  {status:<width$}  {}",
            change.id,
            change.updated_at.format("%Y-%m-%d"),
        );
    }
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
