//! Opening a change: `Store::propose`.
//!
//! Two properties carry this file. The write is one operation — a change row
//! and its proposal artifact appear together or not at all — and it is
//! idempotent on `operation_id` the same way every other mutation in this
//! store is, so a retried `propose` after a crash cannot duplicate a change.

use magent_core::{Classification, OperationId, ProposeCommand};
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
