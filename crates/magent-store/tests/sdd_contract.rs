//! Opening a change, specifying it, planning it and archiving it:
//! `Store::propose`, `Store::specify`, `Store::plan`, `Store::archive`.
//!
//! Two properties carry this file. Each write is one operation — a change row
//! and its proposal artifact appear together or not at all, and so do a
//! change's deltas and their scenarios, a plan's tasks, and every delta an
//! archive folds into the live base — and each is idempotent on
//! `operation_id` the same way every other mutation in this store is, so a
//! retry after a crash cannot duplicate state.

use magent_core::{
    ArchiveCommand, ChangeId, ChangeStatus, CheckpointCommand, CheckpointOrigin, Classification,
    DeltaOp, HarnessKind, OperationId, PlanCommand, ProposeCommand, RequirementDraft,
    ScenarioDraft, SpecBinding, SpecifyCommand, StartRunCommand, TaskDone, TaskDraft,
    WorkflowStage,
};
use magent_store::{FactContext, Store, StoreError};
use rusqlite::Connection;

fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("magent.db");
    let store = Store::open(&path).expect("open");
    (dir, path, store)
}

/// A workspace for `propose` to file the change under, resolved the way a
/// real caller would: from a working directory, not invented by hand.
fn context(store: &Store, dir: &std::path::Path) -> FactContext {
    let project = dir.join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    let resolved = store.resolve_workspace_for(&project).expect("resolve");
    FactContext {
        workspace_id: Some(resolved.workspace_id),
        namespace: None,
        ..FactContext::default()
    }
}

fn propose_command(slug: &str) -> ProposeCommand {
    ProposeCommand {
        operation_id: OperationId::new(),
        slug: slug.into(),
        title: "Add a retry budget".into(),
        classification: Classification::Bounded,
        why: "Retries currently have no ceiling and can loop forever.".into(),
        what_changes: vec!["Add a configurable retry budget".into()],
        capabilities: vec!["worker/retry".into()],
        impact: Some("None known.".into()),
        skip_specs: false,
    }
}

/// Long enough to clear `magent-core`'s 50-character floor on a purpose.
const PURPOSE: &str = "Retrying work that failed for a reason that may not repeat, without \
                       hammering a service that is already struggling.";

fn scenario(name: &str) -> ScenarioDraft {
    ScenarioDraft {
        name: name.into(),
        given: None,
        when: "the budget is exhausted".into(),
        then: "the job is parked".into(),
    }
}

/// An `Added` requirement carrying two scenarios, in the order given.
fn added(name: &str) -> RequirementDraft {
    RequirementDraft {
        op: DeltaOp::Added,
        name: name.into(),
        text: Some("The worker SHALL stop retrying once the budget is spent.".into()),
        rename_to: None,
        reason: None,
        migration: None,
        requirement_id: None,
        scenarios: vec![scenario("first"), scenario("second")],
    }
}

/// A `Modified` requirement, addressed by name.
fn modified(name: &str) -> RequirementDraft {
    RequirementDraft {
        op: DeltaOp::Modified,
        name: name.into(),
        text: Some("The worker SHALL stop retrying once the budget is spent.".into()),
        rename_to: None,
        reason: None,
        migration: None,
        requirement_id: None,
        scenarios: vec![scenario("first")],
    }
}

/// A `Removed` requirement, addressed by name.
fn removed(name: &str) -> RequirementDraft {
    RequirementDraft {
        op: DeltaOp::Removed,
        name: name.into(),
        text: None,
        rename_to: None,
        reason: Some("The budget subsumes it.".into()),
        migration: Some("Set the budget to 1 for the old behaviour.".into()),
        requirement_id: None,
        scenarios: Vec::new(),
    }
}

/// A `Renamed` requirement, addressed by name.
fn renamed(name: &str, rename_to: &str) -> RequirementDraft {
    RequirementDraft {
        op: DeltaOp::Renamed,
        name: name.into(),
        text: None,
        rename_to: Some(rename_to.into()),
        reason: None,
        migration: None,
        requirement_id: None,
        scenarios: Vec::new(),
    }
}

fn specify_command(
    change: ChangeId,
    capability_path: &str,
    purpose: Option<&str>,
    requirements: Vec<RequirementDraft>,
) -> SpecifyCommand {
    SpecifyCommand {
        operation_id: OperationId::new(),
        change,
        capability_path: capability_path.into(),
        purpose: purpose.map(Into::into),
        requirements,
    }
}

/// Capabilities and requirements have no store method yet — creating them is
/// slice 2's archive step — so the rows these tests need are seeded straight
/// through a connection, the way
/// `a_slug_is_free_again_once_the_change_holding_it_is_archived` sets a status.
fn seed_capability(connection: &Connection, workspace_id: &str, id: &str, path: &str) {
    connection
        .execute(
            "INSERT INTO capabilities (id, workspace_id, namespace, path, purpose, created_at, updated_at)
             VALUES (?1, ?2, NULL, ?3, ?4, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            rusqlite::params![id, workspace_id, path, PURPOSE],
        )
        .expect("seed capability");
}

/// `status` is `live` or `removed`: a retired requirement is kept rather than
/// deleted, so a delta can still name one that nothing may patch.
fn seed_requirement(
    connection: &Connection,
    capability_id: &str,
    id: &str,
    name: &str,
    status: &str,
) {
    connection
        .execute(
            "INSERT INTO requirements (id, capability_id, name, text, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'The worker SHALL retry.', ?4, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            rusqlite::params![id, capability_id, name, status],
        )
        .expect("seed requirement");
}

/// A scenario already live against a seeded requirement, so that a delta
/// replacing one can be told from a delta adding one beside it.
fn seed_scenario(connection: &Connection, requirement_id: &str, seq: i64, name: &str) {
    connection
        .execute(
            "INSERT INTO scenarios (id, requirement_id, seq, name, given_text, when_text, then_text)
             VALUES (?1, ?2, ?3, ?4, NULL, 'the budget is exhausted', 'the job is parked')",
            rusqlite::params![format!("{requirement_id}-scenario-{seq}"), requirement_id, seq, name],
        )
        .expect("seed scenario");
}

fn workspace_id(context: &FactContext) -> String {
    context
        .workspace_id
        .expect("the fixture context names a workspace")
        .to_string()
}

fn change_row(connection: &Connection, id: &str) -> (String, String, String) {
    connection
        .query_row(
            "SELECT slug, title, status FROM sdd_changes WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("change row")
}

/// The change's proposal document, parsed. `UNIQUE(change_id, kind)` makes
/// this a single row even after the proposal has been rewritten.
fn proposal_body(connection: &Connection, change_id: &str) -> serde_json::Value {
    let body_json: String = connection
        .query_row(
            "SELECT body_json FROM sdd_artifacts WHERE change_id = ?1 AND kind = 'proposal'",
            [change_id],
            |row| row.get(0),
        )
        .expect("proposal row");
    serde_json::from_str(&body_json).expect("valid json")
}

fn count(connection: &Connection, table: &str, slug_or_id_column: &str, value: &str) -> i64 {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {slug_or_id_column} = ?1"),
            [value],
            |row| row.get(0),
        )
        .expect("count")
}

// --- creating ----------------------------------------------------------

#[test]
fn propose_writes_a_drafting_change_and_a_proposal_artifact() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let command = propose_command("add-retry-budget");

    let change_id = store.propose(&command, &ctx).expect("propose").id;

    let raw = Connection::open(&path).expect("raw connection");
    let (slug, title, status) = change_row(&raw, &change_id.to_string());
    assert_eq!(slug, "add-retry-budget");
    assert_eq!(title, "Add a retry budget");
    assert_eq!(status, "drafting");

    let (kind, body_json): (String, String) = raw
        .query_row(
            "SELECT kind, body_json FROM sdd_artifacts WHERE change_id = ?1",
            [change_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("artifact row");
    assert_eq!(kind, "proposal");

    let body: serde_json::Value = serde_json::from_str(&body_json).expect("valid json");
    assert_eq!(
        body["why"],
        "Retries currently have no ceiling and can loop forever."
    );
    assert_eq!(body["what_changes"][0], "Add a configurable retry budget");
    assert_eq!(body["capabilities"][0], "worker/retry");
    assert_eq!(body["impact"], "None known.");
}

// --- idempotency ---------------------------------------------------------

#[test]
fn a_repeated_operation_id_with_the_same_body_returns_the_same_change_and_writes_nothing_twice() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let command = propose_command("add-retry-budget");

    let first = store.propose(&command, &ctx).expect("first propose").id;
    let second = store.propose(&command, &ctx).expect("replayed propose").id;

    assert_eq!(first, second);

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "sdd_changes", "slug", "add-retry-budget"),
        1,
        "the replay must not have inserted a second change row"
    );
    assert_eq!(
        count(&raw, "sdd_artifacts", "change_id", &first.to_string()),
        1,
        "the replay must not have inserted a second artifact row"
    );
}

#[test]
fn a_repeated_operation_id_with_a_changed_body_is_an_idempotency_conflict() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let mut command = propose_command("add-retry-budget");

    store.propose(&command, &ctx).expect("first propose");

    command.title = "A different title".into();
    let result = store.propose(&command, &ctx);

    assert!(
        matches!(result, Err(StoreError::IdempotencyConflict(id)) if id == command.operation_id),
        "expected an idempotency conflict, got {result:?}"
    );
}

// --- slug occupancy, and rewriting a proposal ------------------------------

/// An author who reaches the specify phase and finds the proposal named the
/// wrong capability has to be able to say so. Refusing the second `propose`
/// would leave that author with no way to declare it and no way to specify it
/// — the work simply stops.
#[test]
fn a_second_propose_on_a_drafting_change_rewrites_its_proposal() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let first = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("first propose")
        .id;

    let mut rewrite = propose_command("add-retry-budget");
    rewrite.title = "Cap the retry loop and drain the queue".into();
    rewrite.classification = Classification::Architectural;
    rewrite.capabilities = vec!["worker/retry".into(), "worker/queue".into()];

    let second = store.propose(&rewrite, &ctx).expect("rewritten propose").id;

    assert_eq!(
        first, second,
        "a rewrite corrects the change that is there, it does not open a second one"
    );

    let raw = Connection::open(&path).expect("raw connection");
    let (_, title, status) = change_row(&raw, &first.to_string());
    assert_eq!(title, "Cap the retry loop and drain the queue");
    assert_eq!(status, "drafting");

    let classification: String = raw
        .query_row(
            "SELECT classification FROM sdd_changes WHERE id = ?1",
            [first.to_string()],
            |row| row.get(0),
        )
        .expect("classification");
    assert_eq!(classification, "architectural");

    let body = proposal_body(&raw, &first.to_string());
    assert_eq!(body["capabilities"][0], "worker/retry");
    assert_eq!(body["capabilities"][1], "worker/queue");

    assert_eq!(
        count(&raw, "sdd_changes", "slug", "add-retry-budget"),
        1,
        "the rewrite must not have opened a second change"
    );
    assert_eq!(
        count(&raw, "sdd_artifacts", "change_id", &first.to_string()),
        1,
        "the proposal is overwritten, not versioned beside itself"
    );
}

/// Past `specified` the proposal has already produced a plan, and rewriting it
/// underneath would move the agreement the plan was written against.
#[test]
fn a_slug_held_by_a_change_already_planned_is_refused() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("first propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET status = 'planned' WHERE id = ?1",
        [change.to_string()],
    )
    .expect("plan");

    let result = store.propose(&propose_command("add-retry-budget"), &ctx);

    assert!(
        matches!(&result, Err(StoreError::SlugTaken(slug)) if slug == "add-retry-budget"),
        "expected SlugTaken, got {result:?}"
    );
}

/// The one thing a rewrite may not do is strand what the author already wrote.
/// A delta cannot be withdrawn, so a proposal that stops declaring the
/// capability holding one leaves that delta pointing at nothing agreed.
#[test]
fn a_rewrite_dropping_a_capability_that_already_has_deltas_is_refused() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let mut proposal = propose_command("add-retry-budget");
    proposal.capabilities = vec!["worker/retry".into(), "worker/queue".into()];
    let change = store.propose(&proposal, &ctx).expect("propose").id;

    store
        .specify(
            &specify_command(
                change,
                "worker/retry",
                Some(PURPOSE),
                vec![added("a spent budget parks the job")],
            ),
            &ctx,
        )
        .expect("specify");

    // The title moves too, so the assertion below tells a refused rewrite from
    // one that landed rather than merely from one that changed nothing.
    let mut rewrite = propose_command("add-retry-budget");
    rewrite.title = "Drain the queue instead".into();
    rewrite.capabilities = vec!["worker/queue".into()];
    let result = store.propose(&rewrite, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::CapabilityDeltasStranded(paths))
                if paths == &["worker/retry".to_string()]
        ),
        "expected the stranded capability to be named, got {result:?}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    let (_, title, _) = change_row(&raw, &change.to_string());
    assert_eq!(
        title, "Add a retry budget",
        "a refused rewrite leaves the proposal as it stood"
    );
    assert_eq!(
        count(&raw, "spec_deltas", "change_id", &change.to_string()),
        1,
        "and leaves the delta it refused to strand"
    );
}

#[test]
fn a_slug_is_free_again_once_the_change_holding_it_is_archived() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let first = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("first propose")
        .id;

    // Archiving is task 4's Store::specify-adjacent surface, not built yet.
    // Set it directly through a raw connection, the way migration_e2e.rs
    // seeds legacy fixtures.
    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET status = 'archived' WHERE id = ?1",
        [first.to_string()],
    )
    .expect("archive");
    drop(raw);

    let second = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("second propose, after archiving")
        .id;

    assert_ne!(first, second);
}

// --- validation --------------------------------------------------------

#[test]
fn an_invalid_command_is_rejected_before_anything_is_written() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let mut command = propose_command("add-retry-budget");
    command.capabilities = Vec::new();
    command.skip_specs = false;

    let result = store.propose(&command, &ctx);
    assert!(
        matches!(result, Err(StoreError::Domain(_))),
        "expected a domain validation error, got {result:?}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    let total: i64 = raw
        .query_row("SELECT COUNT(*) FROM sdd_changes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(total, 0, "validation must fail before any row is written");
}

/// A context with no workspace is answered, not left to the schema.
///
/// This is reachable rather than theoretical: the MCP server builds its
/// context with `resolve_workspace_for(root).ok()`, so any directory that
/// fails to resolve arrives here as `None`. Left to the `NOT NULL` column,
/// the caller would be told a constraint failed — true, and useless, since
/// what needs fixing is the working directory.
#[test]
fn a_context_without_a_workspace_is_named_rather_than_left_to_the_column() {
    let (dir, path, store) = temp_store();
    let _ = dir;

    let context = FactContext {
        workspace_id: None,
        namespace: None,
        ..FactContext::default()
    };

    let result = store.propose(&propose_command("add-retry-budget"), &context);
    assert!(
        matches!(result, Err(StoreError::NoWorkspace)),
        "expected the workspace to be named as missing, got {result:?}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    let total: i64 = raw
        .query_row("SELECT COUNT(*) FROM sdd_changes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(total, 0, "nothing may be written without a workspace");
}

/// Validation must not reach for the writer lock.
///
/// The test above cannot tell where `validate` sits: an error returned from
/// inside `execute_operation` rolls the transaction back, so "no rows" holds
/// either way. The difference is only visible while somebody else is writing.
/// `execute_operation` opens with `BEGIN IMMEDIATE`, so validation placed
/// inside it would queue behind the other writer and spend the five-second
/// busy timeout before failing — and fail with a database error rather than
/// saying what was wrong with the command.
#[test]
fn an_invalid_command_is_rejected_without_waiting_for_the_writer_lock() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    // Held for the duration of the call below, so the writer lock is
    // genuinely unavailable rather than merely contended.
    let blocker = Connection::open(&path).expect("blocker");
    blocker
        .execute_batch("BEGIN IMMEDIATE; CREATE TABLE lock_probe (id INTEGER);")
        .expect("hold the writer lock");

    let mut command = propose_command("add-retry-budget");
    command.capabilities = Vec::new();
    command.skip_specs = false;

    let result = store.propose(&command, &ctx);

    blocker.execute_batch("ROLLBACK").expect("release");

    assert!(
        matches!(result, Err(StoreError::Domain(_))),
        "a malformed command must be named as such without queueing behind a \
         writer; got {result:?}"
    );
}

// --- specifying ----------------------------------------------------------

#[test]
fn specify_writes_a_delta_with_its_scenarios_and_moves_the_change_to_specified() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let command = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let report = store.specify(&command, &ctx).expect("specify");

    assert_eq!(report.capability_path, "worker/retry");
    assert_eq!(report.added, 1);
    assert_eq!(report.modified, 0);
    assert_eq!(report.removed, 0);
    assert_eq!(report.renamed, 0);
    assert_eq!(report.status, ChangeStatus::Specified);

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "spec_deltas", "change_id", &change.to_string()),
        1
    );

    let (delta_id, op, name, capability_id): (String, String, String, Option<String>) = raw
        .query_row(
            "SELECT id, op, name, capability_id FROM spec_deltas WHERE change_id = ?1",
            [change.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("delta row");
    assert_eq!(op, "added");
    assert_eq!(name, "a spent budget parks the job");
    assert_eq!(
        capability_id, None,
        "a capability that does not exist yet cannot be pointed at"
    );

    let scenarios: Vec<(i64, String)> = raw
        .prepare("SELECT seq, name FROM delta_scenarios WHERE delta_id = ?1 ORDER BY seq")
        .expect("prepare")
        .query_map([&delta_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("scenario rows");
    assert_eq!(
        scenarios,
        vec![(0, "first".to_string()), (1, "second".to_string())],
        "the scenarios must keep the order they were written in"
    );

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(status, "specified");
}

/// The point of moving specs into rows: a multi-requirement artifact is
/// indivisible. A markdown file half-written by a crashed process still parses;
/// a change whose first delta landed and whose second was rejected would be a
/// spec nobody wrote and nobody reviewed.
#[test]
fn a_rejected_requirement_takes_the_whole_specify_with_it() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![
            added("a spent budget parks the job"),
            modified("the budget is configurable"),
        ],
    );
    let result = store.specify(&command, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::RequirementNotFound { requirement_id, .. })
                if requirement_id == "requirement-that-never-existed"
        ),
        "expected the dangling requirement id to be named, got {result:?}"
    );

    assert_eq!(
        count(&raw, "spec_deltas", "change_id", &change.to_string()),
        0,
        "the accepted first requirement must not survive the rejected second"
    );
    let scenarios: i64 = raw
        .query_row("SELECT COUNT(*) FROM delta_scenarios", [], |row| row.get(0))
        .expect("count");
    assert_eq!(scenarios, 0, "no scenario may outlive its delta");

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(
        status, "drafting",
        "a failed specify does not advance the change"
    );
}

#[test]
fn a_repeated_specify_returns_the_same_report_and_writes_nothing_twice() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let command = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let first = store.specify(&command, &ctx).expect("first specify");
    let second = store.specify(&command, &ctx).expect("replayed specify");

    assert_eq!(first, second);

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "spec_deltas", "change_id", &change.to_string()),
        1,
        "the replay must not have inserted a second delta"
    );
    let scenarios: i64 = raw
        .query_row("SELECT COUNT(*) FROM delta_scenarios", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        scenarios, 2,
        "the replay must not have inserted second scenarios"
    );
}

#[test]
fn a_modified_delta_naming_a_requirement_of_this_capability_is_accepted() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(
        &raw,
        "cap-retry",
        "req-budget",
        "a budget caps retries",
        "live",
    );

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![modified("a budget caps retries")],
    );
    let report = store.specify(&command, &ctx).expect("specify");

    assert_eq!(report.modified, 1);
    assert_eq!(report.added, 0);

    let (requirement_id, capability_id): (Option<String>, Option<String>) = raw
        .query_row(
            "SELECT requirement_id, capability_id FROM spec_deltas WHERE change_id = ?1",
            [change.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("delta row");
    assert_eq!(requirement_id.as_deref(), Some("req-budget"));
    assert_eq!(
        capability_id.as_deref(),
        Some("cap-retry"),
        "an existing capability is pointed at, not re-created on archive"
    );
}

#[test]
fn a_modified_delta_naming_another_capabilitys_requirement_is_rejected() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    let workspace = workspace_id(&ctx);
    seed_capability(&raw, &workspace, "cap-retry", "worker/retry");
    seed_capability(&raw, &workspace, "cap-queue", "worker/queue");
    seed_requirement(
        &raw,
        "cap-queue",
        "req-queue-depth",
        "the queue has a depth",
        "live",
    );

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![modified("the queue has a depth")],
    );
    let result = store.specify(&command, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::RequirementNotFound { requirement_id, capability_path })
                if requirement_id == "req-queue-depth" && capability_path == "worker/retry"
        ),
        "a requirement of another capability must be named as not belonging here, got {result:?}"
    );
}

#[test]
fn a_modified_delta_resolves_its_requirement_by_name() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(
        &raw,
        "cap-retry",
        "req-budget",
        "a budget caps retries",
        "live",
    );

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![modified("a budget caps retries")],
    );
    let report = store.specify(&command, &ctx).expect("specify");

    assert_eq!(report.modified, 1);

    let stored: Option<String> = raw
        .query_row(
            "SELECT requirement_id FROM spec_deltas WHERE change_id = ?1",
            [change.to_string()],
            |row| row.get(0),
        )
        .expect("delta row");
    assert_eq!(
        stored.as_deref(),
        Some("req-budget"),
        "the delta still stores the id it resolved to; only the caller stopped supplying it"
    );
}

#[test]
fn a_new_capability_without_a_purpose_is_rejected() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![added("a spent budget parks the job")],
    );
    let result = store.specify(&command, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::CapabilityPurposeRequired(path)) if path == "worker/retry"
        ),
        "expected the missing purpose to be named, got {result:?}"
    );
}

#[test]
fn an_existing_capability_carrying_a_purpose_is_rejected_rather_than_ignored() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");

    let command = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let result = store.specify(&command, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::CapabilityPurposeRedundant(path)) if path == "worker/retry"
        ),
        "a purpose written for a capability that already has one must not vanish \
         silently, got {result:?}"
    );
}

#[test]
fn specify_on_an_archived_change_is_rejected() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET status = 'archived' WHERE id = ?1",
        [change.to_string()],
    )
    .expect("archive");

    let command = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let result = store.specify(&command, &ctx);

    assert!(
        matches!(&result, Err(StoreError::ChangeClosed(id)) if *id == change),
        "expected a closed change to be named as closed, got {result:?}"
    );
}

#[test]
fn specify_against_a_change_that_does_not_exist_is_named_rather_than_left_to_the_key() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let missing = ChangeId::new();

    let command = specify_command(
        missing,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let result = store.specify(&command, &ctx);

    assert!(
        matches!(&result, Err(StoreError::ChangeNotFound(id)) if *id == missing),
        "expected the unknown change to be named, got {result:?}"
    );
}

/// A spec that changed under a plan invalidates it, so `specify` pulls the
/// change back to `specified` from any open status rather than only advancing
/// it. Pinned because the behaviour reads like a bug to anyone who has not
/// been told the reasoning: better the next step discovers the plan needs
/// revisiting than the change sits at `planned` carrying a description that
/// plan never covered.
#[test]
fn specify_pulls_a_planned_change_back_to_specified() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    // Planning is slice 2's verb, so the status is set the way archiving is
    // in the test above.
    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET status = 'planned' WHERE id = ?1",
        [change.to_string()],
    )
    .expect("plan");

    let command = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let report = store.specify(&command, &ctx).expect("specify");

    assert_eq!(report.status, ChangeStatus::Specified);
    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(
        status, "specified",
        "a spec written after planning sends the change back to be re-planned"
    );
}

#[test]
fn a_modified_delta_naming_a_removed_requirement_is_rejected() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(
        &raw,
        "cap-retry",
        "req-retired",
        "retries are unbounded",
        "removed",
    );

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![modified("retries are unbounded")],
    );
    let result = store.specify(&command, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::RequirementNotFound { requirement_id, .. })
                if requirement_id == "req-retired"
        ),
        "a requirement already retired cannot be patched, got {result:?}"
    );
}

/// Reached on the ordinary path: a model refining a spec calls `specify`
/// again and repeats a requirement name it already sent. Left to
/// `spec_deltas_identity`, the answer is "UNIQUE constraint failed", which is
/// the message the other checks in this method exist to avoid.
#[test]
fn a_requirement_name_this_change_already_proposed_is_named_rather_than_left_to_the_index() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let first = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    store.specify(&first, &ctx).expect("first specify");

    // A fresh operation_id, so this is a second call rather than a replay.
    let again = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let result = store.specify(&again, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::DeltaAlreadyProposed { requirement_name, capability_path })
                if requirement_name == "a spent budget parks the job"
                    && capability_path == "worker/retry"
        ),
        "expected the repeated requirement name to be named, got {result:?}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "spec_deltas", "change_id", &change.to_string()),
        1,
        "the first call's delta stands; the second added nothing"
    );
}

/// The complement of the test above: a second `specify` adds to what the
/// change already proposes rather than replacing it.
#[test]
fn a_second_specify_adds_to_the_deltas_already_proposed() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let first = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    store.specify(&first, &ctx).expect("first specify");

    let second = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("the budget is configurable")],
    );
    let report = store.specify(&second, &ctx).expect("second specify");

    assert_eq!(
        report.added, 1,
        "the report counts this call, not the change"
    );

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "spec_deltas", "change_id", &change.to_string()),
        2,
        "the earlier delta is kept alongside the new one"
    );
}

// --- the proposal as a contract -------------------------------------------

/// `OpenSpec` calls the proposal's Capabilities section the contract between
/// the proposal and the specs. Without this check a spec can be filed against
/// a capability nobody ever agreed to touch, and nothing later notices.
#[test]
fn specify_naming_a_capability_the_proposal_never_declared_is_refused() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let command = specify_command(
        change,
        "never/declared-in-the-proposal",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let result = store.specify(&command, &ctx);

    let Err(StoreError::CapabilityNotProposed {
        capability_path,
        declared,
    }) = &result
    else {
        panic!("expected the undeclared capability to be named, got {result:?}");
    };
    assert_eq!(capability_path, "never/declared-in-the-proposal");
    assert_eq!(declared, &["worker/retry".to_string()]);

    // Naming only the offending path would send the caller to read the
    // proposal out of the database to find out what it may write instead.
    let message = result.as_ref().expect_err("refused").to_string();
    assert!(
        message.contains("worker/retry"),
        "the refusal must list what the proposal does declare, got {message:?}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "spec_deltas", "change_id", &change.to_string()),
        0,
        "nothing may be written against a capability nobody proposed"
    );
}

/// The other half of the contract: the ordinary path still works. A check that
/// refuses the declared capability too would be indistinguishable from one
/// that refuses everything.
#[test]
fn specify_naming_a_declared_capability_is_accepted() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let command = specify_command(
        change,
        "worker/retry",
        Some(PURPOSE),
        vec![added("a spent budget parks the job")],
    );
    let report = store.specify(&command, &ctx).expect("specify");

    assert_eq!(report.capability_path, "worker/retry");
    assert_eq!(report.added, 1);
}

/// The contract above and the rewrite in `propose` are one mechanism: without
/// the rewrite, an author who discovers a missing capability at the specify
/// phase has no way to declare it and the work stops here.
#[test]
fn a_capability_added_by_a_rewritten_proposal_can_then_be_specified() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let mut rewrite = propose_command("add-retry-budget");
    rewrite.capabilities = vec!["worker/retry".into(), "worker/queue".into()];
    store.propose(&rewrite, &ctx).expect("rewritten propose");

    let command = specify_command(
        change,
        "worker/queue",
        Some(PURPOSE),
        vec![added("the queue drains before shutdown")],
    );
    let report = store.specify(&command, &ctx).expect("specify");

    assert_eq!(
        report.capability_path, "worker/queue",
        "a capability the rewrite declared is one the change may now specify"
    );
}

/// Moving the capability list moves the contract, so the change goes back.
///
/// The specs were written against the scope the proposal named. A rewrite
/// that changes that scope leaves them describing something nobody agreed
/// to — the same reasoning that pulls a `planned` change back to `specified`
/// when its spec moves.
#[test]
fn a_rewrite_that_changes_the_capabilities_returns_the_change_to_drafting() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(status, "specified", "the fixture starts specified");

    let mut widened = propose_command("add-retry-budget");
    widened.capabilities = vec!["worker/retry".into(), "worker/queue".into()];
    store.propose(&widened, &ctx).expect("rewrite");

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(
        status, "drafting",
        "a change of scope has to be specified again"
    );
}

/// The same capabilities in another order are the same contract.
///
/// Compared positionally, a proposal restating its own list would read as a
/// change of scope and cost a re-specification nobody asked for.
#[test]
fn a_rewrite_that_only_reorders_the_capabilities_leaves_the_status_alone() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");

    let mut opening = propose_command("add-retry-budget");
    opening.capabilities = vec!["worker/retry".into(), "worker/queue".into()];
    let change = store.propose(&opening, &ctx).expect("propose").id;
    store
        .specify(
            &specify_command(
                change,
                "worker/retry",
                Some(PURPOSE),
                vec![added(REQUIREMENT)],
            ),
            &ctx,
        )
        .expect("specify");

    let mut reordered = propose_command("add-retry-budget");
    reordered.capabilities = vec!["worker/queue".into(), "worker/retry".into()];
    store.propose(&reordered, &ctx).expect("rewrite");

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(
        status, "specified",
        "the same set in another order is the same contract"
    );
}

/// Correcting the prose invalidates nothing.
#[test]
fn a_rewrite_that_only_fixes_the_wording_leaves_the_status_alone() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    let mut reworded = propose_command("add-retry-budget");
    reworded.title = "Cap the retry loop".into();
    store.propose(&reworded, &ctx).expect("rewrite");

    let (_, title, status) = change_row(&raw, &change.to_string());
    assert_eq!(title, "Cap the retry loop", "the new title is recorded");
    assert_eq!(
        status, "specified",
        "a better sentence is not a change of scope"
    );
}

/// The identifier alone cannot tell an author what just happened, and by the
/// time a proposal is rewritten there is something to tell: whether this
/// opened a change or corrected one, and whether the correction cost the
/// specs already written against it.
#[test]
fn a_propose_reports_whether_it_opened_a_change_or_rewrote_one() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let opened = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose");
    assert_eq!(opened.slug, "add-retry-budget");
    assert_eq!(opened.status, ChangeStatus::Drafting);
    assert!(!opened.rewritten, "nothing stood under this slug");

    let mut widened = propose_command("add-retry-budget");
    widened.capabilities = vec!["worker/retry".into(), "worker/queue".into()];
    let rewritten = store.propose(&widened, &ctx).expect("rewrite");

    assert_eq!(
        rewritten.id, opened.id,
        "a rewrite corrects the change that is there"
    );
    assert!(rewritten.rewritten);
    assert_eq!(
        rewritten.status,
        ChangeStatus::Drafting,
        "the status the caller has to act on is the one the rewrite left behind"
    );
}

// --- planning ------------------------------------------------------------

/// One task of a plan, covering the requirement names it is given.
fn task(number: &str, covers: &[&str]) -> TaskDraft {
    TaskDraft {
        number: number.into(),
        title: format!("Cap the retry loop, step {number}"),
        body: Some("Read the budget from config and stop once it is spent.".into()),
        files: vec!["crates/worker/src/retry.rs".into()],
        consumes: None,
        produces: Some("fn spend_budget(&mut self) -> bool".into()),
        verify_command: "cargo test -p worker retry".into(),
        expected_output: vec!["test result: ok".into(), "3 passed".into()],
        covers: covers.iter().map(|name| (*name).to_string()).collect(),
    }
}

fn plan_command(change: ChangeId, tasks: Vec<TaskDraft>) -> PlanCommand {
    PlanCommand {
        operation_id: OperationId::new(),
        change,
        tasks,
    }
}

/// A change carrying one `Added` requirement and sitting at `specified` —
/// the state a plan is normally written from.
fn specified_change(store: &Store, ctx: &FactContext, slug: &str, requirement: &str) -> ChangeId {
    let change = store
        .propose(&propose_command(slug), ctx)
        .expect("propose")
        .id;
    store
        .specify(
            &specify_command(
                change,
                "worker/retry",
                Some(PURPOSE),
                vec![added(requirement)],
            ),
            ctx,
        )
        .expect("specify");
    change
}

/// The ids of a change's tasks, in plan order — enough to tell a replay that
/// wrote nothing from one that deleted the rows and wrote them again.
fn task_ids(connection: &Connection, change_id: &str) -> Vec<String> {
    connection
        .prepare("SELECT id FROM tasks WHERE change_id = ?1 ORDER BY number")
        .expect("prepare")
        .query_map([change_id], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("task ids")
}

const REQUIREMENT: &str = "a spent budget parks the job";

#[test]
fn plan_writes_the_tasks_and_moves_the_change_to_planned() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    let command = plan_command(
        change,
        vec![task("1", &[REQUIREMENT]), task("2.1", &[REQUIREMENT])],
    );
    let report = store.plan(&command, &ctx).expect("plan");

    assert_eq!(report.tasks, 2);
    assert_eq!(report.status, ChangeStatus::Planned);

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(count(&raw, "tasks", "change_id", &change.to_string()), 2);

    let (number, verify_command, expected_output_json, covers_json, status): (
        String,
        String,
        String,
        String,
        String,
    ) = raw
        .query_row(
            "SELECT number, verify_command, expected_output_json, covers_json, status
             FROM tasks WHERE change_id = ?1 AND number = '2.1'",
            [change.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("task row");
    assert_eq!(number, "2.1");
    assert_eq!(verify_command, "cargo test -p worker retry");
    assert_eq!(expected_output_json, r#"["test result: ok","3 passed"]"#);
    assert_eq!(status, "pending");

    let covers: Vec<String> = serde_json::from_str(&covers_json).expect("covers is json");
    assert_eq!(covers, vec![REQUIREMENT.to_string()]);

    let (_, _, change_status) = change_row(&raw, &change.to_string());
    assert_eq!(change_status, "planned");
}

/// The refusal names what is uncovered. "Coverage is incomplete" leaves the
/// caller to run the query itself against a plan it has just been told is
/// wrong — and the names are the only part of the answer it cannot derive
/// from the command it sent.
#[test]
fn a_requirement_no_task_covers_is_named_in_the_refusal() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    let command = plan_command(change, vec![task("1", &[])]);
    let error = store.plan(&command, &ctx).expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::RequirementsUncovered(names) if names == &[REQUIREMENT]),
        "expected the uncovered requirement to be listed, got {error:?}"
    );
    assert!(
        error.to_string().contains(REQUIREMENT),
        "the message must name what is uncovered, got {error}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "tasks", "change_id", &change.to_string()),
        0,
        "a refused plan writes no tasks"
    );
    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(status, "specified", "a refused plan does not advance");
}

/// A plan is one artifact. Half of it is not a shorter plan but a plan whose
/// numbering and dependencies nobody checked, so a task that cannot be
/// written takes every task beside it with it.
#[test]
fn a_task_that_cannot_be_written_takes_the_whole_plan_with_it() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    // The second task is made unwritable from outside the store: nothing
    // about the command itself is wrong, so the failure lands mid-write
    // rather than during validation.
    let raw = Connection::open(&path).expect("raw connection");
    raw.execute_batch(
        "CREATE TRIGGER refuse_the_second_task BEFORE INSERT ON tasks
         WHEN NEW.number = '2'
         BEGIN SELECT RAISE(ABORT, 'the second task cannot be written'); END;",
    )
    .expect("trigger");

    let command = plan_command(
        change,
        vec![task("1", &[REQUIREMENT]), task("2", &[REQUIREMENT])],
    );
    let error = store.plan(&command, &ctx).expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::Database(message)
            if message.contains("the second task cannot be written")),
        "expected the write to fail, got {error:?}"
    );

    assert_eq!(
        count(&raw, "tasks", "change_id", &change.to_string()),
        0,
        "the accepted first task must not survive the rejected second"
    );
    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(
        status, "specified",
        "a failed plan does not advance the change"
    );
}

/// Planning again replaces the plan rather than adding to it — unlike
/// `specify`, which accumulates. A plan's numbers and dependencies are agreed
/// against each other, so a task appended to the side of one belongs to no
/// plan anybody reviewed.
#[test]
fn replanning_replaces_the_previous_tasks_rather_than_adding_to_them() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    store
        .plan(
            &plan_command(
                change,
                vec![task("1", &[REQUIREMENT]), task("2", &[REQUIREMENT])],
            ),
            &ctx,
        )
        .expect("first plan");

    // A fresh operation_id, so this is a second call rather than a replay.
    let report = store
        .plan(&plan_command(change, vec![task("1", &[REQUIREMENT])]), &ctx)
        .expect("replan");

    assert_eq!(report.tasks, 1);

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        count(&raw, "tasks", "change_id", &change.to_string()),
        1,
        "the earlier plan is replaced, not added to"
    );
}

#[test]
fn planning_a_change_that_was_never_specified_is_refused() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    let command = plan_command(change, vec![task("1", &[])]);
    let result = store.plan(&command, &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::ChangeNotSpecified { change: id, status })
                if *id == change && status == "drafting"
        ),
        "expected an unspecified change to be named as such, got {result:?}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(count(&raw, "tasks", "change_id", &change.to_string()), 0);
}

/// A change that deliberately skips specs never reaches `specified`, so
/// requiring that status would leave it with no way to be planned at all.
#[test]
fn a_change_that_skips_specs_is_planned_straight_from_drafting() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let mut proposal = propose_command("retire-the-old-runner");
    proposal.capabilities = Vec::new();
    proposal.skip_specs = true;
    let change = store.propose(&proposal, &ctx).expect("propose").id;

    let report = store
        .plan(&plan_command(change, vec![task("1", &[])]), &ctx)
        .expect("a change without specs has nothing to leave uncovered");

    assert_eq!(report.tasks, 1);
    assert_eq!(report.status, ChangeStatus::Planned);

    let raw = Connection::open(&path).expect("raw connection");
    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(status, "planned");
}

#[test]
fn a_repeated_plan_returns_the_same_report_and_writes_nothing_twice() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    let command = plan_command(
        change,
        vec![task("1", &[REQUIREMENT]), task("2", &[REQUIREMENT])],
    );
    let first = store.plan(&command, &ctx).expect("first plan");

    let raw = Connection::open(&path).expect("raw connection");
    let before = task_ids(&raw, &change.to_string());

    let second = store.plan(&command, &ctx).expect("replayed plan");

    assert_eq!(first, second);
    assert_eq!(
        count(&raw, "tasks", "change_id", &change.to_string()),
        2,
        "the replay must not have inserted more tasks"
    );
    assert_eq!(
        task_ids(&raw, &change.to_string()),
        before,
        "the replay must not have rewritten the tasks under new ids"
    );
}

#[test]
fn plan_on_an_archived_change_is_rejected() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET status = 'archived' WHERE id = ?1",
        [change.to_string()],
    )
    .expect("archive");

    let result = store.plan(&plan_command(change, vec![task("1", &[REQUIREMENT])]), &ctx);

    assert!(
        matches!(&result, Err(StoreError::ChangeClosed(id)) if *id == change),
        "expected a closed change to be named as closed, got {result:?}"
    );
}

#[test]
fn planning_a_change_that_does_not_exist_is_named_rather_than_left_to_the_key() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let absent = ChangeId::new();

    let result = store.plan(&plan_command(absent, vec![task("1", &[REQUIREMENT])]), &ctx);

    assert!(
        matches!(&result, Err(StoreError::ChangeNotFound(id)) if *id == absent),
        "expected the change to be named as missing, got {result:?}"
    );
}

/// A change under execution can be replanned, and doing so replaces its plan.
///
/// This was refused once, on the grounds that a task carries the evidence its
/// verification ran and replacing it would delete that record. `task_ticks`
/// holds the evidence now, keyed to the change rather than to the task row, so
/// the plan is free to change and what was proved under the old one stays
/// proved. A plan that turns out wrong halfway is the ordinary case, and it is
/// what the executing skill tells its reader to do.
#[test]
fn planning_a_change_already_being_executed_replaces_its_plan() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    store
        .plan(&plan_command(change, vec![task("1", &[REQUIREMENT])]), &ctx)
        .expect("the first plan lands");

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET status = 'executing' WHERE id = ?1",
        [change.to_string()],
    )
    .expect("start executing");

    store
        .plan(&plan_command(change, vec![task("9", &[REQUIREMENT])]), &ctx)
        .expect("a plan that turns out wrong halfway has to be correctable");

    let (number,): (String,) = raw
        .query_row(
            "SELECT number FROM tasks WHERE change_id = ?1",
            [change.to_string()],
            |row| Ok((row.get(0)?,)),
        )
        .expect("the new plan's task");
    assert_eq!(
        number, "9",
        "replanning replaces the plan rather than adding to it"
    );
}

// --- archiving -----------------------------------------------------------

fn archive_command(change: ChangeId) -> ArchiveCommand {
    ArchiveCommand {
        operation_id: OperationId::new(),
        change,
    }
}

/// Every task of a change closed, the way executing the plan would leave
/// them. Marking a task done is not a verb this store has yet, so the rows
/// are set the way `a_slug_is_free_again_once_the_change_holding_it_is_archived`
/// sets a status.
fn finish_tasks(connection: &Connection, change_id: &str) {
    connection
        .execute(
            "UPDATE tasks SET status = 'done' WHERE change_id = ?1",
            [change_id],
        )
        .expect("finish tasks");
}

/// A change specified with these requirements, planned with one task covering
/// all of them and that task done: the state `archive` is called from.
fn archivable_change(
    store: &Store,
    ctx: &FactContext,
    connection: &Connection,
    slug: &str,
    purpose: Option<&str>,
    requirements: Vec<RequirementDraft>,
) -> ChangeId {
    let change = store
        .propose(&propose_command(slug), ctx)
        .expect("propose")
        .id;
    let covers: Vec<String> = requirements
        .iter()
        .map(|requirement| requirement.name.clone())
        .collect();

    store
        .specify(
            &specify_command(change, "worker/retry", purpose, requirements),
            ctx,
        )
        .expect("specify");

    let covers: Vec<&str> = covers.iter().map(String::as_str).collect();
    store
        .plan(&plan_command(change, vec![task("1", &covers)]), ctx)
        .expect("plan");
    finish_tasks(connection, &change.to_string());

    change
}

/// A requirement's live scenarios, in sequence order.
fn live_scenarios(connection: &Connection, requirement_id: &str) -> Vec<(i64, String)> {
    connection
        .prepare("SELECT seq, name FROM scenarios WHERE requirement_id = ?1 ORDER BY seq")
        .expect("prepare")
        .query_map([requirement_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("scenario rows")
}

/// The three text columns of a live scenario, read back in order.
///
/// `when_text` and `then_text` are adjacent columns of the same type, so a
/// reordered `SELECT` in the copy would swap them and every assertion on
/// `(seq, name)` alone would stay green.
fn live_scenario_text(
    connection: &Connection,
    requirement_id: &str,
) -> Vec<(Option<String>, String, String)> {
    connection
        .prepare(
            "SELECT given_text, when_text, then_text FROM scenarios
             WHERE requirement_id = ?1 ORDER BY seq",
        )
        .expect("prepare")
        .query_map([requirement_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("scenario text")
}

fn requirement_row(connection: &Connection, id: &str) -> (String, String, String) {
    connection
        .query_row(
            "SELECT name, text, status FROM requirements WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("requirement row")
}

/// How much of the live specification exists at all. Zero until an archive
/// folds something in, so a refused one is measured by it staying there.
fn live_requirement_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM requirements", [], |row| row.get(0))
        .expect("requirement count")
}

/// Closes one task the way executing a plan closes it: a run bound to the
/// change by its slug, carrying a tick with the command the plan named.
///
/// Unlike `finish_tasks`, which sets the column, this is the path that also
/// moves the change to `ready` — the state the coverage hole below is reached
/// from. The run is started from the same `project` directory `context`
/// resolved its workspace from, or the tick would be looking for the change in
/// another workspace's plan.
fn close_task_by_checkpoint(store: &Store, dir: &std::path::Path, slug: &str, number: &str) {
    let started = store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: "Cap the retry loop".into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![dir.join("project")],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("start run");

    store
        .save_checkpoint(&CheckpointCommand {
            operation_id: OperationId::new(),
            run_id: started.run_id,
            session_id: started.session_id,
            stage: WorkflowStage::Executing,
            origin: CheckpointOrigin::Enriched,
            completed_steps: vec!["capped the retry loop".into()],
            next_steps: vec![],
            decisions: vec![],
            rejected: vec![],
            changed_files: vec![],
            verification: vec![],
            risks: vec![],
            handoff_summary: "The budget is read from config and spent per attempt.".into(),
            task_done: Some(TaskDone {
                number: number.into(),
                verify_command: "cargo test -p worker retry".into(),
                output: "running 3 tests\ntest result: ok. 3 passed\n".into(),
            }),
            binding: Some(SpecBinding {
                change_id: Some(slug.into()),
                current_task: Some(format!("{number}: cap the retry loop")),
            }),
        })
        .expect("checkpoint");
}

/// The step the rest of the process exists for: until a change is archived
/// its deltas are only a proposal, and the live base still describes the
/// product as it was.
#[test]
fn archiving_an_added_delta_writes_the_requirement_its_scenarios_and_its_capability() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    let change = archivable_change(
        &store,
        &ctx,
        &raw,
        "add-retry-budget",
        Some(PURPOSE),
        vec![added(REQUIREMENT)],
    );

    let report = store
        .archive(&archive_command(change), &ctx)
        .expect("archive");

    assert_eq!(report.added, 1);
    assert_eq!(report.modified, 0);
    assert_eq!(report.removed, 0);
    assert_eq!(report.renamed, 0);
    assert_eq!(
        report.capabilities_created,
        vec!["worker/retry".to_string()],
        "a capability nothing had created yet is named as created here"
    );
    assert_eq!(report.status, ChangeStatus::Archived);

    let (capability_id, purpose): (String, String) = raw
        .query_row(
            "SELECT id, purpose FROM capabilities WHERE path = 'worker/retry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("capability row");
    assert_eq!(
        purpose, PURPOSE,
        "the purpose the delta carried is what the new capability keeps"
    );

    let requirement_id: String = raw
        .query_row(
            "SELECT id FROM requirements WHERE capability_id = ?1",
            [&capability_id],
            |row| row.get(0),
        )
        .expect("requirement row");
    let (name, text, status) = requirement_row(&raw, &requirement_id);
    assert_eq!(name, REQUIREMENT);
    assert_eq!(
        text,
        "The worker SHALL stop retrying once the budget is spent."
    );
    assert_eq!(status, "live");

    assert_eq!(
        live_scenarios(&raw, &requirement_id),
        vec![(0, "first".to_string()), (1, "second".to_string())],
        "the scenarios keep the sequence the delta wrote them in"
    );

    assert_eq!(
        live_scenario_text(&raw, &requirement_id),
        vec![
            (
                None,
                "the budget is exhausted".into(),
                "the job is parked".into()
            ),
            (
                None,
                "the budget is exhausted".into(),
                "the job is parked".into()
            ),
        ],
        "when and then are adjacent columns of one type: read them back or a \
         swap in the copy goes unnoticed"
    );

    let (_, _, change_status) = change_row(&raw, &change.to_string());
    assert_eq!(change_status, "archived");

    // The other side of the two coverage refusals below: a change every delta
    // of which a done task covers is folded in, not held back by that gate.
    assert_eq!(
        live_requirement_count(&raw),
        1,
        "a fully covered change still archives, and its delta lands"
    );
}

/// A `modified` delta carries the whole requirement rather than a diff, so
/// archiving it replaces the scenarios outright. Counted rather than probed
/// for the new one: a scenario list that grew by one is a requirement
/// describing two behaviours where its author wrote one.
#[test]
fn archiving_a_modified_delta_replaces_the_text_and_the_scenarios_wholesale() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(
        &raw,
        "cap-retry",
        "req-budget",
        "a budget caps retries",
        "live",
    );
    seed_scenario(&raw, "req-budget", 0, "stale");

    let change = archivable_change(
        &store,
        &ctx,
        &raw,
        "add-retry-budget",
        None,
        vec![modified("a budget caps retries")],
    );

    let report = store
        .archive(&archive_command(change), &ctx)
        .expect("archive");

    assert_eq!(report.modified, 1);
    assert!(
        report.capabilities_created.is_empty(),
        "the capability already existed"
    );

    let (name, text, status) = requirement_row(&raw, "req-budget");
    assert_eq!(name, "a budget caps retries");
    assert_eq!(
        text,
        "The worker SHALL stop retrying once the budget is spent."
    );
    assert_eq!(status, "live");

    assert_eq!(
        live_scenarios(&raw, "req-budget"),
        vec![(0, "first".to_string())],
        "the delta's scenarios replace what stood before rather than joining it"
    );
}

/// Two deltas cannot give one new capability two different purposes.
///
/// Neither `specify` call can refuse the second purpose: the capability does
/// not exist yet either time, so both are legitimately required to carry one.
/// The clash only becomes visible at archive, where the first delta creates
/// the row — and keeping whichever sorted first would drop text somebody
/// wrote and report success.
#[test]
fn two_deltas_giving_one_new_capability_different_purposes_are_refused() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");

    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    store
        .specify(
            &specify_command(change, "worker/retry", Some(PURPOSE), vec![added("first")]),
            &ctx,
        )
        .expect("the capability does not exist, so a purpose is required");

    let other = "Something else entirely, written by someone who did not know \
                 the capability was already being introduced next door.";
    store
        .specify(
            &specify_command(change, "worker/retry", Some(other), vec![added("second")]),
            &ctx,
        )
        .expect("still no row, so this purpose is required too");

    store
        .plan(
            &plan_command(change, vec![task("1", &["first", "second"])]),
            &ctx,
        )
        .expect("plan");
    finish_tasks(&raw, &change.to_string());

    let result = store.archive(&archive_command(change), &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::CapabilityPurposeRedundant(path)) if path == "worker/retry"
        ),
        "expected the clash to be named, got {result:?}"
    );
    assert_eq!(
        count(&raw, "capabilities", "path", "worker/retry"),
        0,
        "nothing may be written when the two disagree"
    );
}

/// A requirement retired between `specify` and `archive` is not patched.
///
/// `specify` checks the target is live, but archiving is what moves the live
/// base, and the gap between the two calls is wide enough for another change
/// to have been proposed, specified and archived — retiring this very
/// requirement. Left unguarded the update lands anyway, leaving a retired row
/// carrying text nobody agreed to ship.
#[test]
fn archiving_a_modified_delta_whose_target_was_retired_meanwhile_is_refused() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(
        &raw,
        "cap-retry",
        "req-budget",
        "a budget caps retries",
        "live",
    );

    let change = archivable_change(
        &store,
        &ctx,
        &raw,
        "add-retry-budget",
        None,
        vec![modified("a budget caps retries")],
    );

    // Another change got there first.
    raw.execute(
        "UPDATE requirements SET status = 'removed' WHERE id = 'req-budget'",
        [],
    )
    .expect("retire it");

    let result = store.archive(&archive_command(change), &ctx);

    assert!(
        matches!(
            &result,
            Err(StoreError::RequirementNotFound { requirement_id, .. })
                if requirement_id == "req-budget"
        ),
        "expected the retired target to be named, got {result:?}"
    );

    let (_, text, status) = requirement_row(&raw, "req-budget");
    assert_eq!(status, "removed", "it must stay retired");
    assert_eq!(
        text, "The worker SHALL retry.",
        "a retired requirement must not be rewritten"
    );
}

/// Kept rather than deleted: a retired requirement is the record of a
/// decision, and `requirements.status` exists for exactly this.
#[test]
fn archiving_a_removed_delta_retires_the_requirement_without_deleting_it() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(
        &raw,
        "cap-retry",
        "req-budget",
        "a budget caps retries",
        "live",
    );

    let change = archivable_change(
        &store,
        &ctx,
        &raw,
        "add-retry-budget",
        None,
        vec![removed("a budget caps retries")],
    );

    let report = store
        .archive(&archive_command(change), &ctx)
        .expect("archive");

    assert_eq!(report.removed, 1);
    assert_eq!(
        count(&raw, "requirements", "id", "req-budget"),
        1,
        "the row stays; only its status moves"
    );
    let (name, _, status) = requirement_row(&raw, "req-budget");
    assert_eq!(name, "a budget caps retries");
    assert_eq!(status, "removed");
}

#[test]
fn archiving_a_renamed_delta_moves_the_name_and_leaves_the_text_and_scenarios() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(
        &raw,
        "cap-retry",
        "req-budget",
        "a budget caps retries",
        "live",
    );
    seed_scenario(&raw, "req-budget", 0, "stale");

    let change = archivable_change(
        &store,
        &ctx,
        &raw,
        "add-retry-budget",
        None,
        vec![renamed(
            "a budget caps retries",
            "retries are capped by a budget",
        )],
    );

    let report = store
        .archive(&archive_command(change), &ctx)
        .expect("archive");

    assert_eq!(report.renamed, 1);

    let (name, text, status) = requirement_row(&raw, "req-budget");
    assert_eq!(name, "retries are capped by a budget");
    assert_eq!(
        text, "The worker SHALL retry.",
        "a rename does not touch what the requirement says"
    );
    assert_eq!(status, "live");
    assert_eq!(
        live_scenarios(&raw, "req-budget"),
        vec![(0, "stale".to_string())],
        "a rename does not touch the scenarios either"
    );
}

/// Archiving files the change's spec as what is now true, so a task nobody
/// finished would put an unwritten behaviour into the live base. The refusal
/// names the numbers: "something is open" sends the caller to re-run the
/// query against a plan it has only been told is incomplete.
#[test]
fn archiving_a_change_with_an_open_task_names_that_task() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    store
        .plan(
            &plan_command(
                change,
                vec![task("1", &[REQUIREMENT]), task("2.1", &[REQUIREMENT])],
            ),
            &ctx,
        )
        .expect("plan");

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE tasks SET status = 'done' WHERE change_id = ?1 AND number = '1'",
        [change.to_string()],
    )
    .expect("finish the first task");

    let error = store
        .archive(&archive_command(change), &ctx)
        .expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::ChangeNotExecuted { change: id, tasks }
            if *id == change && tasks == &["2.1".to_string()]),
        "expected the open task to be listed, got {error:?}"
    );
    assert!(
        error.to_string().contains("2.1"),
        "the message must name the open task, got {error}"
    );

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(status, "planned", "a refused archive does not advance");
}

/// Coverage is checked once, when the plan is written, and nothing holds it
/// afterwards: `specify` adds a requirement to a change that is already
/// planned without touching its plan. So a requirement can reach this gate
/// having never been anybody's task — every task done, the change reading
/// `ready`, and a behaviour nobody implemented about to be filed as true.
#[test]
fn archiving_refuses_a_requirement_no_finished_task_covers() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    store
        .plan(&plan_command(change, vec![task("1", &[REQUIREMENT])]), &ctx)
        .expect("plan");
    close_task_by_checkpoint(&store, dir.path(), "add-retry-budget", "1");

    let late = "the budget is configurable";
    store
        .specify(
            &specify_command(change, "worker/retry", Some(PURPOSE), vec![added(late)]),
            &ctx,
        )
        .expect("the second specify");

    let error = store
        .archive(&archive_command(change), &ctx)
        .expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::RequirementsUnimplemented(names)
            if names == &[late.to_string()]),
        "expected the requirement no finished task covers to be named, got {error:?}"
    );

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        live_requirement_count(&raw),
        0,
        "a refused archive folds nothing into the live specification"
    );
}

/// `require_tasks_closed` counts a skipped task as closed, because passing one
/// over is a decision somebody took. Coverage cannot count it: a task nobody
/// did implements nothing, whoever decided that.
#[test]
fn a_skipped_task_covers_nothing() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    store
        .plan(&plan_command(change, vec![task("1", &[REQUIREMENT])]), &ctx)
        .expect("plan");

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE tasks SET status = 'skipped' WHERE change_id = ?1",
        [change.to_string()],
    )
    .expect("skip the only task");

    let error = store
        .archive(&archive_command(change), &ctx)
        .expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::RequirementsUnimplemented(names)
            if names == &[REQUIREMENT.to_string()]),
        "expected the skipped task's requirement to be named, got {error:?}"
    );
    assert_eq!(
        live_requirement_count(&raw),
        0,
        "a refused archive folds nothing into the live specification"
    );
}

/// A change with specs but no plan was never executed either. It reaches the
/// same refusal as one with an open task, because it is the same fault: the
/// spec is about to be filed as true and nothing did the work.
#[test]
fn archiving_a_change_that_was_never_planned_is_refused() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    let error = store
        .archive(&archive_command(change), &ctx)
        .expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::ChangeNotExecuted { change: id, tasks }
            if *id == change && tasks.is_empty()),
        "expected a change with no tasks to be refused, got {error:?}"
    );
}

/// Archiving is what makes a change's deltas true. One carrying none, and not
/// declaring that it never would, has nothing to fold in — and moving it to
/// `archived` regardless would quietly lose it.
#[test]
fn archiving_a_change_that_proposes_no_deltas_is_refused() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    // Planned and done, so the task check is not what refuses this.
    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET status = 'specified' WHERE id = ?1",
        [change.to_string()],
    )
    .expect("specified");
    store
        .plan(&plan_command(change, vec![task("1", &[])]), &ctx)
        .expect("plan");
    finish_tasks(&raw, &change.to_string());

    let error = store
        .archive(&archive_command(change), &ctx)
        .expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::NothingToArchive(id) if *id == change),
        "expected an empty change to be named as such, got {error:?}"
    );

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(status, "planned", "a refused archive does not close it");
}

/// The complement: a change proposed with `skip_specs` legitimately has
/// neither deltas nor, if nothing was planned, tasks. Archiving it closes it
/// and folds in nothing.
#[test]
fn a_change_that_skips_specs_archives_with_nothing_to_fold_in() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let mut proposal = propose_command("retire-the-old-runner");
    proposal.capabilities = Vec::new();
    proposal.skip_specs = true;
    let change = store.propose(&proposal, &ctx).expect("propose").id;

    let report = store
        .archive(&archive_command(change), &ctx)
        .expect("archive");

    assert_eq!(report.added, 0);
    assert_eq!(report.status, ChangeStatus::Archived);
    assert!(report.capabilities_created.is_empty());

    let raw = Connection::open(&path).expect("raw connection");
    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(status, "archived");
}

#[test]
fn a_repeated_archive_returns_the_same_report_and_applies_the_deltas_once() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    let change = archivable_change(
        &store,
        &ctx,
        &raw,
        "add-retry-budget",
        Some(PURPOSE),
        vec![added(REQUIREMENT)],
    );

    let command = archive_command(change);
    let first = store.archive(&command, &ctx).expect("first archive");

    let requirements: i64 = raw
        .query_row("SELECT COUNT(*) FROM requirements", [], |row| row.get(0))
        .expect("count");
    assert_eq!(requirements, 1);

    let second = store.archive(&command, &ctx).expect("replayed archive");

    assert_eq!(first, second);
    let after: i64 = raw
        .query_row("SELECT COUNT(*) FROM requirements", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        after, requirements,
        "the replay must not have applied the deltas a second time"
    );
    let scenarios: i64 = raw
        .query_row("SELECT COUNT(*) FROM scenarios", [], |row| row.get(0))
        .expect("count");
    assert_eq!(scenarios, 2, "nor their scenarios");
}

/// The live base is what the next change reads as its starting point, so a
/// half-applied archive is worse than none: it describes a product that never
/// existed, and nothing downstream can tell which half landed.
#[test]
fn a_delta_that_cannot_be_applied_leaves_no_trace_of_the_ones_before_it() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let raw = Connection::open(&path).expect("raw connection");
    let change = archivable_change(
        &store,
        &ctx,
        &raw,
        "add-retry-budget",
        Some(PURPOSE),
        vec![
            added("a spent budget parks the job"),
            added("b the budget is configurable"),
        ],
    );

    // Made unwritable from outside the store: nothing about the command is
    // wrong, so the failure lands mid-write rather than during validation.
    // The deltas are applied in name order, so this is the second of the two.
    raw.execute_batch(
        "CREATE TRIGGER refuse_the_second_requirement BEFORE INSERT ON requirements
         WHEN NEW.name = 'b the budget is configurable'
         BEGIN SELECT RAISE(ABORT, 'the second requirement cannot be written'); END;",
    )
    .expect("trigger");

    let error = store
        .archive(&archive_command(change), &ctx)
        .expect_err("expected a refusal");

    assert!(
        matches!(&error, StoreError::Database(message)
            if message.contains("the second requirement cannot be written")),
        "expected the write to fail, got {error:?}"
    );

    let requirements: i64 = raw
        .query_row("SELECT COUNT(*) FROM requirements", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        requirements, 0,
        "the accepted first requirement must not survive the rejected second"
    );
    let scenarios: i64 = raw
        .query_row("SELECT COUNT(*) FROM scenarios", [], |row| row.get(0))
        .expect("count");
    assert_eq!(scenarios, 0, "no scenario may outlive its requirement");
    let capabilities: i64 = raw
        .query_row("SELECT COUNT(*) FROM capabilities", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        capabilities, 0,
        "nor may the capability the first delta created"
    );

    let (_, _, status) = change_row(&raw, &change.to_string());
    assert_eq!(
        status, "planned",
        "a failed archive does not close the change"
    );
}

// --- reading ---------------------------------------------------------------

/// A second workspace, resolved from a different directory the way `context`
/// resolves the first — so a change filed under one is genuinely invisible to
/// the other rather than merely asked about with a different label.
fn other_workspace_context(store: &Store, dir: &std::path::Path) -> FactContext {
    let project = dir.join("other-project");
    std::fs::create_dir_all(&project).expect("mkdir");
    let resolved = store.resolve_workspace_for(&project).expect("resolve");
    FactContext {
        workspace_id: Some(resolved.workspace_id),
        namespace: None,
        ..FactContext::default()
    }
}

#[test]
fn open_changes_returns_open_changes_freshest_first_and_excludes_archived() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let older = store
        .propose(&propose_command("older-change"), &ctx)
        .expect("propose older")
        .id;
    let newer = store
        .propose(&propose_command("newer-change"), &ctx)
        .expect("propose newer")
        .id;
    let archived = store
        .propose(&propose_command("archived-change"), &ctx)
        .expect("propose archived")
        .id;

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE sdd_changes SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params!["2026-01-01T00:00:00Z", older.to_string()],
    )
    .expect("set older's timestamp");
    raw.execute(
        "UPDATE sdd_changes SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params!["2026-01-02T00:00:00Z", newer.to_string()],
    )
    .expect("set newer's timestamp");
    raw.execute(
        "UPDATE sdd_changes SET status = 'archived', updated_at = ?1 WHERE id = ?2",
        rusqlite::params!["2026-01-03T00:00:00Z", archived.to_string()],
    )
    .expect("archive it, later than either open change");

    let changes = store.open_changes(&ctx).expect("open_changes");
    let ids: Vec<ChangeId> = changes.iter().map(|change| change.id).collect();

    assert_eq!(
        ids,
        vec![newer, older],
        "freshest first, and the archived change does not appear at all"
    );
}

#[test]
fn open_changes_counts_deltas_and_tasks_correctly() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    store
        .specify(
            &specify_command(
                change,
                "worker/retry",
                Some(PURPOSE),
                vec![added("a spent budget parks the job")],
            ),
            &ctx,
        )
        .expect("specify one delta");
    store
        .specify(
            &specify_command(
                change,
                "worker/retry",
                Some(PURPOSE),
                vec![added("the budget is configurable")],
            ),
            &ctx,
        )
        .expect("specify a second delta");

    store
        .plan(
            &plan_command(
                change,
                vec![
                    task("1", &["a spent budget parks the job"]),
                    task("2", &["the budget is configurable"]),
                    task("3", &[]),
                ],
            ),
            &ctx,
        )
        .expect("plan");

    let raw = Connection::open(&path).expect("raw connection");
    raw.execute(
        "UPDATE tasks SET status = 'done' WHERE change_id = ?1 AND number = '1'",
        [change.to_string()],
    )
    .expect("finish one task");

    let changes = store.open_changes(&ctx).expect("open_changes");
    let summary = changes
        .into_iter()
        .find(|change_summary| change_summary.id == change)
        .expect("the change is in its own workspace's list");

    assert_eq!(summary.delta_count, 2, "two specify calls, two deltas");
    assert_eq!(summary.task_count, 3, "the plan has three tasks");
    assert_eq!(
        summary.tasks_closed, 1,
        "exactly one of the three tasks is done"
    );
}

#[test]
fn change_detail_reads_the_proposal_deltas_and_tasks() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose")
        .id;

    store
        .specify(
            &specify_command(
                change,
                "worker/retry",
                Some(PURPOSE),
                vec![added(REQUIREMENT)],
            ),
            &ctx,
        )
        .expect("specify");

    store
        .plan(&plan_command(change, vec![task("1", &[REQUIREMENT])]), &ctx)
        .expect("plan");

    let detail = store
        .change_detail(change, &ctx)
        .expect("change_detail")
        .expect("the change exists in this workspace");

    assert_eq!(detail.id, change);
    assert_eq!(detail.slug, "add-retry-budget");
    assert_eq!(
        detail.why, "Retries currently have no ceiling and can loop forever.",
        "the proposal body must actually be read back, not left empty"
    );
    assert_eq!(
        detail.capabilities,
        vec!["worker/retry".to_string()],
        "the capabilities the proposal declared must be read back too"
    );

    assert_eq!(detail.deltas.len(), 1);
    assert_eq!(detail.deltas[0].op, DeltaOp::Added);
    assert_eq!(detail.deltas[0].name, REQUIREMENT);
    assert_eq!(detail.deltas[0].capability_path, "worker/retry");

    assert_eq!(detail.tasks.len(), 1);
    assert_eq!(detail.tasks[0].number, "1");
    assert_eq!(detail.tasks[0].status, "pending");
    assert_eq!(detail.tasks[0].verify_command, "cargo test -p worker retry");
}

#[test]
fn change_detail_for_an_unknown_change_is_ok_none_not_an_error() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let missing = ChangeId::new();

    let result = store
        .change_detail(missing, &ctx)
        .expect("an unknown change must not be an error");

    assert!(
        result.is_none(),
        "asking about a change that does not exist is a legitimate question"
    );
}

/// A change from another workspace is not "someone else's change" to either
/// method — it does not exist as far as this workspace is concerned, the same
/// way `require_open_change` treats it for the write side.
#[test]
fn a_change_from_another_workspace_is_invisible_to_open_changes_and_change_detail() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let other_ctx = other_workspace_context(&store, dir.path());

    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose in the first workspace")
        .id;

    let changes = store
        .open_changes(&other_ctx)
        .expect("open_changes in the other workspace");
    assert!(
        changes
            .iter()
            .all(|change_summary| change_summary.id != change),
        "a change from another workspace must not appear in this one's list"
    );

    let detail = store
        .change_detail(change, &other_ctx)
        .expect("change_detail in the other workspace");
    assert!(
        detail.is_none(),
        "a change from another workspace must not be readable through this one"
    );
}

/// The two coverage gates ask different questions and a caller has to be able
/// to tell which one refused it. `magent_plan` says the plan in hand is
/// incomplete — write another task. `magent_archive` says the work is not
/// finished — close one. Under a single code the caller cannot tell the two
/// apart, and the archive-side message reads falsely for the skipped case,
/// where a task covering the requirement demonstrably exists.
#[test]
fn the_two_coverage_refusals_do_not_share_a_code() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    // Planning with a task that covers nothing: the plan itself is incomplete.
    let at_plan = store
        .plan(&plan_command(change, vec![task("1", &[])]), &ctx)
        .expect_err("expected a refusal");
    assert_eq!(at_plan.code(), "requirements_uncovered", "{at_plan:?}");

    // Now a complete plan, left unexecuted: the plan is fine, the work is not.
    store
        .plan(&plan_command(change, vec![task("1", &[REQUIREMENT])]), &ctx)
        .expect("plan");
    let raw = Connection::open(dir.path().join("magent.db")).expect("raw connection");
    raw.execute(
        "UPDATE tasks SET status = 'skipped' WHERE change_id = ?1",
        [change.to_string()],
    )
    .expect("skip the only task");

    let at_archive = store
        .archive(&archive_command(change), &ctx)
        .expect_err("expected a refusal");
    assert_eq!(
        at_archive.code(),
        "requirements_unimplemented",
        "{at_archive:?}"
    );
    assert!(
        at_archive.to_string().contains(REQUIREMENT),
        "the refusal names what is unimplemented: {at_archive}"
    );
    assert!(
        at_archive.to_string().contains("close"),
        "a refusal says what to do instead: {at_archive}"
    );
}

/// The trap the archive gate would otherwise set, on the exact path the change
/// that added it describes: plan two tasks, tick one, specify a requirement the
/// plan does not cover, tick the other. The last tick sends the change to
/// `ready` — and only then does archiving refuse, naming a requirement whose
/// fix is a plan. So planning has to be possible from `ready`, or the refusal
/// names a way out that is itself refused.
///
/// `ready` was refused for one stated reason: a replan deletes the tasks, and
/// with them the evidence of work already verified. `task_ticks` is that
/// evidence now, and no plan can delete it — so the reason is gone, and with it
/// the refusal.
#[test]
fn a_ready_change_can_still_be_replanned() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = specified_change(&store, &ctx, "add-retry-budget", REQUIREMENT);

    store
        .plan(
            &plan_command(
                change,
                vec![task("1", &[REQUIREMENT]), task("2", &[REQUIREMENT])],
            ),
            &ctx,
        )
        .expect("plan");
    close_task_by_checkpoint(&store, dir.path(), "add-retry-budget", "1");

    let late = "the budget is configurable";
    store
        .specify(
            &specify_command(change, "worker/retry", Some(PURPOSE), vec![added(late)]),
            &ctx,
        )
        .expect("the second specify");

    // The tick that leaves no task open, so the change reads `ready` while a
    // requirement nobody planned for stands uncovered.
    close_task_by_checkpoint(&store, dir.path(), "add-retry-budget", "2");

    let refused = store
        .archive(&archive_command(change), &ctx)
        .expect_err("the late requirement is covered by no finished task");
    assert_eq!(refused.code(), "requirements_unimplemented", "{refused:?}");

    // The way out the refusal names. This is the step that was refused.
    store
        .plan(
            &plan_command(change, vec![task("1", &[REQUIREMENT]), task("2", &[late])]),
            &ctx,
        )
        .expect("a change told to plan a task has to be able to plan one");

    close_task_by_checkpoint(&store, dir.path(), "add-retry-budget", "1");
    close_task_by_checkpoint(&store, dir.path(), "add-retry-budget", "2");
    store
        .archive(&archive_command(change), &ctx)
        .expect("every requirement is covered by a finished task now");

    let raw = Connection::open(&path).expect("raw connection");
    assert_eq!(
        live_requirement_count(&raw),
        2,
        "both requirements were implemented, so both are now specification"
    );
    let ticks: i64 = raw
        .query_row("SELECT COUNT(*) FROM task_ticks", [], |row| row.get(0))
        .expect("count the ticks");
    assert_eq!(
        ticks, 4,
        "the two ticks taken under the plan that was replaced are still recorded"
    );
}
