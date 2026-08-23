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

/// The workspace seeded into every legacy fixture.
const LEGACY_WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";

/// The run seeded into every legacy fixture.
const LEGACY_RUN: &str = "33333333-3333-4333-8333-333333333333";

/// The change seeded into the fixture that carries a plan.
const LEGACY_CHANGE: &str = "88888888-8888-4888-8888-888888888888";

/// The single line a pre-0011 plan wrote into `tasks.expected_output`.
const LEGACY_EXPECTED_OUTPUT: &str = "test result: ok. 3 passed";

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
        7 => "0007_sdd.sql",
        8 => "0008_drop_orphan_distill_jobs.sql",
        9 => "0009_tasks.sql",
        10 => "0010_drop_spec_paths.sql",
        11 => "0011_expected_output_is_a_list.sql",
        12 => "0012_task_ticks.sql",
        13 => "0013_session_notices.sql",
        14 => "0014_requirement_origin.sql",
        15 => "0015_session_last_seen.sql",
        16 => "0016_contracts_are_lists.sql",
        17 => "0017_task_holds.sql",
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

/// A change, its proposal and one planned task, as slice 3 wrote them before
/// 0011 — `expected_output` still one column of prose, not a JSON list.
fn seed_planned_task(connection: &Connection) {
    connection
        .execute(
            "INSERT INTO sdd_changes
                 (id, workspace_id, namespace, slug, title, classification, status,
                  created_at, updated_at)
             VALUES (?1, ?2, 'service', 'add-retry-budget', 'Give retries a ceiling',
                     'bounded', 'planned', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            (LEGACY_CHANGE, LEGACY_WORKSPACE),
        )
        .expect("seed change");
    connection
        .execute(
            "INSERT INTO sdd_artifacts (id, change_id, kind, body_json, created_at, updated_at)
             VALUES ('99999999-9999-4999-8999-999999999999', ?1, 'proposal',
                     '{\"why\":\"Retries can loop forever.\",
                       \"what_changes\":[\"cap the retries\"],
                       \"capabilities\":[\"worker/retry\"]}',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [LEGACY_CHANGE],
        )
        .expect("seed proposal");
    connection
        .execute(
            "INSERT INTO tasks
                 (id, change_id, number, title, body, files_json, verify_command,
                  expected_output, status, created_at, updated_at)
             VALUES ('aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', ?1, '1', 'cap the retries',
                     'Stop the loop.', '[\"crates/worker/src/retry.rs\"]',
                     'cargo test -p worker retry', ?2, 'pending',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            (LEGACY_CHANGE, LEGACY_EXPECTED_OUTPUT),
        )
        .expect("seed task");
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

/// 0011 is the only migration so far that rewrites a value someone else wrote
/// — 0005's `UPDATE` fills a column it has just added — and it does so twice
/// over, wrapping `expected_output` in `json_array` and then renaming the
/// column. What a plan stated it was looking for is what makes a tick
/// auditable, and a botched rewrite would leave a bare string where the store
/// now parses JSON: a profile planned before the upgrade would refuse to open
/// its own plan, or come back with the wrong marker.
#[test]
fn a_version_ten_profile_keeps_what_its_plan_expected() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 10);
    seed_slice_one(&legacy);
    seed_planned_task(&legacy);
    drop(legacy);

    let store = Store::open(&path).expect("migrate");

    let detail = store
        .change_detail(
            LEGACY_CHANGE.parse().expect("a uuid"),
            &FactContext {
                workspace_id: Some(LEGACY_WORKSPACE.parse().expect("a uuid")),
                ..FactContext::default()
            },
            None,
        )
        .expect("change_detail")
        .expect("the change must still be readable after migrating");

    assert_eq!(
        detail.tasks.len(),
        1,
        "the task was lost: {:?}",
        detail.tasks
    );
    assert_eq!(
        detail.tasks[0].expected_output,
        vec![LEGACY_EXPECTED_OUTPUT.to_string()],
        "the line the plan wrote must survive as a single-element list"
    );
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

#[test]
fn sessions_carry_a_last_seen_stamp_backfilled_from_their_start() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 14);
    seed_slice_one(&legacy);
    drop(legacy);

    Store::open(&path).expect("upgrade");

    let connection = Connection::open(&path).expect("reopen");
    let (started_at, last_seen_at): (String, Option<String>) = connection
        .query_row(
            "SELECT started_at, last_seen_at FROM sessions WHERE id = ?1",
            ["44444444-4444-4444-8444-444444444444"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the sessions row must carry last_seen_at");

    assert_eq!(
        last_seen_at.as_deref(),
        Some(started_at.as_str()),
        "a session that predates the stamp is worth exactly the moment it started"
    );
}

#[test]
fn a_tasks_prose_contract_survives_as_a_single_entry() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 15);
    seed_slice_one(&legacy);

    // Seeded here rather than through `seed_planned_task`, which writes the
    // pre-0011 `expected_output` column and so only builds against version 10.
    legacy
        .execute(
            "INSERT INTO sdd_changes
                 (id, workspace_id, namespace, slug, title, classification, status,
                  created_at, updated_at)
             VALUES (?1, ?2, 'service', 'add-retry-budget', 'Give retries a ceiling',
                     'bounded', 'planned', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            (LEGACY_CHANGE, LEGACY_WORKSPACE),
        )
        .expect("seed change");
    legacy
        .execute(
            "INSERT INTO tasks
                 (id, change_id, number, title, files_json, consumes, produces,
                  verify_command, expected_output_json, covers_json, status,
                  created_at, updated_at)
             VALUES ('aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', ?1, '1', 'cap the retries',
                     '[]', ?2, ?3, 'cargo test -p worker retry', '[\"ok\"]', '[]',
                     'pending', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            (
                LEGACY_CHANGE,
                "RetryBudget::new(u32) from task 1",
                "nothing downstream",
            ),
        )
        .expect("seed a prose contract");
    drop(legacy);

    Store::open(&path).expect("upgrade");

    let connection = Connection::open(&path).expect("reopen");
    let (consumes, produces): (String, String) = connection
        .query_row(
            "SELECT consumes_json, produces_json FROM tasks LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the columns must be lists now");

    assert_eq!(consumes, "[\"RetryBudget::new(u32) from task 1\"]");
    assert_eq!(produces, "[\"nothing downstream\"]");
}

/// A plan written before the shape changed is still executable.
///
/// Its prose became a single entry that matches nothing, and a task waiting on
/// an artifact no task in the plan produces would wait for ever — so the ready
/// set must not count it as a dependency.
#[test]
fn a_migrated_plan_still_offers_its_open_tasks() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 15);
    seed_slice_one(&legacy);
    legacy
        .execute(
            "INSERT INTO sdd_changes
                 (id, workspace_id, namespace, slug, title, classification, status,
                  created_at, updated_at)
             VALUES (?1, ?2, 'service', 'add-retry-budget', 'Give retries a ceiling',
                     'bounded', 'planned', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            (LEGACY_CHANGE, LEGACY_WORKSPACE),
        )
        .expect("seed change");
    legacy
        .execute(
            "INSERT INTO tasks
                 (id, change_id, number, title, files_json, consumes, produces,
                  verify_command, expected_output_json, covers_json, status,
                  created_at, updated_at)
             VALUES ('bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb', ?1, '1', 'cap the retries',
                     '[]', 'struct RetryConfig, from the task before this one', NULL,
                     'cargo test', '[\"ok\"]', '[]', 'pending',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [LEGACY_CHANGE],
        )
        .expect("seed a prose contract");
    drop(legacy);

    let store = Store::open(&path).expect("upgrade");
    let ready = store
        .ready_tasks(LEGACY_CHANGE.parse().expect("a change id"), None)
        .expect("ready set");

    assert_eq!(
        ready.len(),
        1,
        "a migrated task must still be offered: {ready:?}"
    );
}

#[test]
fn a_task_carries_who_holds_it_and_until_when() {
    let (_dir, path) = temp();
    let legacy = database_at(&path, 16);
    seed_slice_one(&legacy);
    drop(legacy);

    Store::open(&path).expect("upgrade");

    let connection = Connection::open(&path).expect("reopen");
    let columns: String = connection
        .query_row(
            "SELECT group_concat(name, ' ') FROM pragma_table_info('tasks')",
            [],
            |row| row.get(0),
        )
        .expect("read the columns");

    assert!(
        columns.contains("claimed_by"),
        "tasks must record who holds one: {columns}"
    );
    assert!(columns.contains("lease_until"), "and until when: {columns}");
}
