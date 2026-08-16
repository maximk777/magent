//! Opening a change and specifying it: `Store::propose`, `Store::specify`.
//!
//! Two properties carry this file. Each write is one operation — a change row
//! and its proposal artifact appear together or not at all, and so do a
//! change's deltas and their scenarios — and each is idempotent on
//! `operation_id` the same way every other mutation in this store is, so a
//! retry after a crash cannot duplicate state.

use magent_core::{
    ChangeId, ChangeStatus, Classification, DeltaOp, OperationId, ProposeCommand, RequirementDraft,
    ScenarioDraft, SpecifyCommand,
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

/// A `Modified` requirement pointing at `requirement_id`.
fn modified(name: &str, requirement_id: &str) -> RequirementDraft {
    RequirementDraft {
        op: DeltaOp::Modified,
        name: name.into(),
        text: Some("The worker SHALL stop retrying once the budget is spent.".into()),
        rename_to: None,
        reason: None,
        migration: None,
        requirement_id: Some(requirement_id.into()),
        scenarios: vec![scenario("first")],
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

fn seed_requirement(connection: &Connection, capability_id: &str, id: &str, name: &str) {
    connection
        .execute(
            "INSERT INTO requirements (id, capability_id, name, text, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'The worker SHALL retry.', 'live', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            rusqlite::params![id, capability_id, name],
        )
        .expect("seed requirement");
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

    let change_id = store.propose(&command, &ctx).expect("propose");

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

    let first = store.propose(&command, &ctx).expect("first propose");
    let second = store.propose(&command, &ctx).expect("replayed propose");

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

// --- slug occupancy --------------------------------------------------------

#[test]
fn a_slug_held_by_a_live_change_is_rejected_with_a_meaningful_error() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());

    store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("first propose");

    let result = store.propose(&propose_command("add-retry-budget"), &ctx);

    assert!(
        matches!(&result, Err(StoreError::SlugTaken(slug)) if slug == "add-retry-budget"),
        "expected SlugTaken, got {result:?}"
    );
}

#[test]
fn a_slug_is_free_again_once_the_change_holding_it_is_archived() {
    let (dir, path, store) = temp_store();
    let ctx = context(&store, dir.path());

    let first = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("first propose");

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
        .expect("second propose, after archiving");

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
        .expect("propose");

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
        .expect("propose");

    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![
            added("a spent budget parks the job"),
            modified(
                "the budget is configurable",
                "requirement-that-never-existed",
            ),
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
        .expect("propose");

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
        .expect("propose");

    let raw = Connection::open(&path).expect("raw connection");
    seed_capability(&raw, &workspace_id(&ctx), "cap-retry", "worker/retry");
    seed_requirement(&raw, "cap-retry", "req-budget", "a budget caps retries");

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![modified("a budget caps retries", "req-budget")],
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
        .expect("propose");

    let raw = Connection::open(&path).expect("raw connection");
    let workspace = workspace_id(&ctx);
    seed_capability(&raw, &workspace, "cap-retry", "worker/retry");
    seed_capability(&raw, &workspace, "cap-queue", "worker/queue");
    seed_requirement(
        &raw,
        "cap-queue",
        "req-queue-depth",
        "the queue has a depth",
    );

    let command = specify_command(
        change,
        "worker/retry",
        None,
        vec![modified("a budget caps retries", "req-queue-depth")],
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
fn a_new_capability_without_a_purpose_is_rejected() {
    let (dir, _path, store) = temp_store();
    let ctx = context(&store, dir.path());
    let change = store
        .propose(&propose_command("add-retry-budget"), &ctx)
        .expect("propose");

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
        .expect("propose");

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
        .expect("propose");

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
