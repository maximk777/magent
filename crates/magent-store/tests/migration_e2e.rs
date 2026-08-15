//! Upgrading a store that already holds someone's work.
//!
//! Every other test starts from an empty database, so the migration chain is
//! only ever exercised on nothing. A real profile has runs, checkpoints and a
//! hundred facts in it, and a migration that drops or mangles them is the one
//! failure with no recovery: the transcripts they were distilled from are long
//! gone.
//!
//! Each case builds a database at an older version, fills it the way that
//! version's code would have, and opens it with the current one.

use std::path::Path;

use magent_core::{
    Cardinality, FactKind, FactScope, FactStatus, HarnessKind, OperationId, RememberCommand,
    StartRunCommand,
};
use magent_store::{CURRENT_VERSION, FactContext, FactQuery, Store, StoreError};
use rusqlite::Connection;

/// The run seeded into every legacy fixture.
const LEGACY_RUN: &str = "33333333-3333-4333-8333-333333333333";

/// The migrations as they shipped. Read from disk rather than re-declared, so a
/// migration edited after release fails these tests instead of passing them.
fn migration(version: i64) -> String {
    let name = match version {
        1 => "0001_slice1.sql",
        2 => "0002_facts.sql",
        3 => "0003_retrieval.sql",
        4 => "0004_grouping.sql",
        5 => "0005_identity.sql",
        6 => "0006_dependencies.sql",
        other => panic!("no migration {other}"),
    };

    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("migrations")
            .join(name),
    )
    .unwrap_or_else(|error| panic!("{name} could not be read: {error}"))
}

/// A database frozen at `version`, as an older build would have left it.
fn database_at(path: &Path, version: i64) -> Connection {
    let connection = Connection::open(path).expect("open");
    connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("wal");
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("foreign_keys");

    for applied in 1..=version {
        connection
            .execute_batch(&migration(applied))
            .unwrap_or_else(|error| panic!("migration {applied} failed: {error}"));
        connection
            .execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                (applied, "2026-01-01T00:00:00Z"),
            )
            .expect("record migration");
    }

    connection
}

/// A run, a session and a checkpoint, as slice 1 wrote them.
fn seed_slice_one(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO workspaces (id, name, created_at)
                 VALUES ('11111111-1111-4111-8111-111111111111', 'legacy', '2026-01-01T00:00:00Z');
             INSERT INTO repositories (id, workspace_id, identity_key, canonical_root, created_at)
                 VALUES ('22222222-2222-4222-8222-222222222222', '11111111-1111-4111-8111-111111111111', 'git:example.invalid/acme/service', '/tmp/service',
                         '2026-01-01T00:00:00Z');
             INSERT INTO runs (id, workspace_id, task, status, stage, created_at, updated_at)
                 VALUES ('33333333-3333-4333-8333-333333333333', '11111111-1111-4111-8111-111111111111', 'fix the payment timeout', 'open', 'executing',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
             INSERT INTO sessions (id, run_id, harness, started_at)
                 VALUES ('44444444-4444-4444-8444-444444444444', '33333333-3333-4333-8333-333333333333', 'claude_code', '2026-01-01T00:00:00Z');
             INSERT INTO checkpoints
                 (id, run_id, session_id, operation_id, stage, origin, payload_json, created_at)
                 VALUES ('55555555-5555-4555-8555-555555555555', '33333333-3333-4333-8333-333333333333', '44444444-4444-4444-8444-444444444444', '66666666-6666-4666-8666-666666666666', 'executing', 'enriched',
                         '{\"operation_id\":\"00000000-0000-4000-8000-000000000001\",
                            \"run_id\":\"00000000-0000-4000-8000-000000000002\",
                            \"session_id\":\"00000000-0000-4000-8000-000000000003\",
                            \"stage\":\"executing\",\"origin\":\"enriched\",
                            \"completed_steps\":[\"traced the owner\"],\"next_steps\":[],
                            \"decisions\":[],\"rejected\":[],\"changed_files\":[],
                            \"verification\":[],\"risks\":[],
                            \"handoff_summary\":\"owner traced\"}',
                         '2026-01-01T00:00:00Z');",
        )
        .expect("seed slice one");
}

/// Facts, as slice 2 wrote them.
fn seed_facts(connection: &Connection) {
    connection
        .execute(
            "INSERT INTO facts
                 (id, name, title, body, kind, scope, cardinality, status, confidence,
                  namespace, provenance, created_at, updated_at)
             VALUES ('77777777-7777-4777-8777-777777777777', 'goose-table-locking', 'goose locks with a table locker',
                     'goose_lock needs DDL rights', 'project', 'repository', 'set',
                     'observed', 0.7, 'service', 'imported',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed facts");
}

fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("magent.db");
    (dir, path)
}

// --- upgrading from every shipped version ----------------------------------

/// The oldest profile anyone could have. Its runs and checkpoints are the whole
/// reason the store exists.
#[test]
fn a_version_one_profile_upgrades_without_losing_its_work() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 1);
    seed_slice_one(&legacy);
    drop(legacy);

    let store = Store::open(&path).expect("open should migrate, not refuse");
    assert_eq!(store.schema_version().expect("version"), CURRENT_VERSION);

    let run = store
        .get_run(LEGACY_RUN.parse().expect("a uuid"))
        .expect("the run must still be addressable after migrating");

    assert_eq!(run.task, "fix the payment timeout");
    assert_eq!(
        run.latest_checkpoint
            .expect("the checkpoint must survive")
            .handoff_summary,
        "owner traced",
        "the checkpoint's contents were mangled"
    );
    assert_eq!(store.run_count().expect("runs"), 1, "the run was lost");
    assert_eq!(
        store.total_checkpoint_count().expect("checkpoints"),
        1,
        "the checkpoint was lost in migration"
    );
}

#[test]
fn a_version_two_profile_keeps_its_facts() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 2);
    seed_slice_one(&legacy);
    seed_facts(&legacy);
    drop(legacy);

    let store = Store::open(&path).expect("migrate");

    let found = store
        .search(&FactQuery {
            text: Some("DDL rights".into()),
            namespaces: vec!["service".into()],
            ..FactQuery::default()
        })
        .expect("search");

    assert_eq!(found.len(), 1, "the fact did not survive: {found:?}");
    assert_eq!(found[0].name, "goose-table-locking");
}

/// 0004 added a unique index on workspace names, and 0005 replaced it with one
/// that only binds explicit groups. A profile that crossed both must end up
/// able to resolve two repositories whose directories share a basename.
#[test]
fn a_version_four_profile_can_still_resolve_colliding_directory_names() {
    let (dir, path) = temp();
    let legacy = database_at(&path, 4);
    seed_slice_one(&legacy);
    drop(legacy);

    let store = Store::open(&path).expect("migrate");

    let first = dir.path().join("alpha").join("api");
    let second = dir.path().join("beta").join("api");
    std::fs::create_dir_all(&first).expect("mkdir");
    std::fs::create_dir_all(&second).expect("mkdir");

    let alpha = store.resolve_workspace_for(&first).expect("alpha");
    let beta = store
        .resolve_workspace_for(&second)
        .expect("two directories called api must both resolve");

    assert_ne!(alpha.repository_id, beta.repository_id);
}

#[test]
fn every_shipped_version_upgrades_to_the_current_one() {
    for version in 1..=CURRENT_VERSION {
        let (_dir, path) = temp();
        let legacy = database_at(&path, version);
        seed_slice_one(&legacy);
        if version >= 2 {
            seed_facts(&legacy);
        }
        drop(legacy);

        let store = Store::open(&path)
            .unwrap_or_else(|error| panic!("a version {version} profile failed to open: {error}"));

        assert_eq!(
            store.schema_version().expect("version"),
            CURRENT_VERSION,
            "a version {version} profile did not reach the current schema"
        );
        assert_eq!(
            store.run_count().expect("runs"),
            1,
            "a version {version} profile lost its run"
        );
    }
}

// --- the store keeps working afterwards ------------------------------------

/// Migrating is only half of it. The upgraded profile has to accept new work,
/// or the damage shows up on the next session instead of during the upgrade.
#[test]
fn an_upgraded_profile_accepts_new_work() {
    let (dir, path) = temp();
    let legacy = database_at(&path, 1);
    seed_slice_one(&legacy);
    drop(legacy);

    let store = Store::open(&path).expect("migrate");

    let started = store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: "something new".into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![dir.path().to_path_buf()],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("a migrated profile must accept a new run");

    store
        .remember(
            &RememberCommand {
                operation_id: OperationId::new(),
                name: "learned-after-upgrade".into(),
                title: "something learned after the upgrade".into(),
                body: "and it should be findable".into(),
                kind: FactKind::Project,
                scope: FactScope::Repository,
                cardinality: Cardinality::Set,
                status: FactStatus::Observed,
                confidence: 0.8,
                evidence: vec![],
                relates_to: vec![],
            },
            &FactContext {
                workspace_id: Some(started.workspace_id),
                namespace: Some("after".into()),
                ..FactContext::default()
            },
        )
        .expect("a migrated profile must accept a new fact");

    assert_eq!(store.run_count().expect("runs"), 2);
    assert_eq!(
        store
            .search(&FactQuery {
                text: Some("findable".into()),
                namespaces: vec!["after".into()],
                ..FactQuery::default()
            })
            .expect("search")
            .len(),
        1
    );
}

// --- refusing what it cannot understand ------------------------------------

/// A profile written by a newer build must be refused, not opened and quietly
/// half-read. Downgrading is how a schema gets corrupted by a binary that
/// thinks it understands it.
#[test]
fn a_newer_profile_is_refused_rather_than_damaged() {
    let (_dir, path) = temp();
    let future = database_at(&path, CURRENT_VERSION);
    future
        .execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            (CURRENT_VERSION + 1, "2027-01-01T00:00:00Z"),
        )
        .expect("pretend a newer build wrote this");
    drop(future);

    match Store::open(&path) {
        Err(StoreError::UnsupportedSchema(version)) => {
            assert_eq!(version, CURRENT_VERSION + 1);
        }
        Err(other) => panic!("expected UnsupportedSchema, got {other}"),
        Ok(_) => panic!("a newer profile was opened instead of refused"),
    }
}

/// Opening the same profile twice must not re-run anything. A migration applied
/// a second time would fail on its own CREATE TABLE at best, and duplicate rows
/// at worst.
#[test]
fn opening_an_already_current_profile_changes_nothing() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 1);
    seed_slice_one(&legacy);
    drop(legacy);

    let first = Store::open(&path).expect("first");
    let version = first.schema_version().expect("version");
    let runs = first.run_count().expect("runs");
    drop(first);

    let second = Store::open(&path).expect("second");
    assert_eq!(second.schema_version().expect("version"), version);
    assert_eq!(second.run_count().expect("runs"), runs);
}

/// Two processes opening an old profile at once is ordinary: a hook and the MCP
/// server start together at the beginning of a session. Only one may migrate.
#[test]
fn concurrent_opens_of_an_old_profile_migrate_once() {
    use std::sync::mpsc;

    let (_dir, path) = temp();
    let legacy = database_at(&path, 1);
    seed_slice_one(&legacy);
    drop(legacy);

    let (sender, receiver) = mpsc::channel();
    for _ in 0..4 {
        let path = path.clone();
        let sender = sender.clone();
        std::thread::spawn(move || {
            let outcome = Store::open(&path).map(|store| store.schema_version());
            let _ = sender.send(outcome.is_ok());
        });
    }
    drop(sender);

    let outcomes: Vec<bool> = receiver.iter().collect();
    assert_eq!(outcomes.len(), 4);
    assert!(
        outcomes.iter().all(|ok| *ok),
        "a concurrent open failed: {outcomes:?}"
    );

    let store = Store::open(&path).expect("open");
    assert_eq!(store.schema_version().expect("version"), CURRENT_VERSION);
    assert_eq!(store.run_count().expect("runs"), 1);
}
