//! Exporting memory back to markdown.
//!
//! This is what keeps the store from being a one-way door. Everything imported
//! must be able to come back out in the format it came from, readable and
//! editable without Magent, or adopting the store means betting the corpus on
//! one binary.

use std::path::Path;

use magent_cli::export::export_memory_dir;
use magent_cli::import::import_memory_dir;
use magent_core::{
    Cardinality, FactKind, FactScope, FactStatus, OperationId, RelationKind, RememberCommand,
};
use magent_store::{FactContext, Store};

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

fn seeded_store(dir: &Path) -> Store {
    let store = Store::open(&dir.join("magent.db")).expect("open");
    store
        .remember(
            &RememberCommand {
                operation_id: OperationId::new(),
                name: "goose-table-locking".into(),
                title: "goose v3.26 locks with NewPostgresTableLocker".into(),
                body: "goose_lock is auto-created and needs DDL rights.".into(),
                kind: FactKind::Project,
                scope: FactScope::Repository,
                cardinality: Cardinality::Set,
                status: FactStatus::Observed,
                confidence: 0.8,
                evidence: vec![],
                relates_to: vec![("goose-migrate-arm-testbed".into(), RelationKind::Related)],
            },
            &FactContext {
                namespace: Some("wb-bank-clients".into()),
                ..FactContext::default()
            },
        )
        .expect("remember");
    store
}

// --- shape -----------------------------------------------------------------

#[test]
fn a_fact_becomes_a_file_under_its_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = seeded_store(dir.path());
    let out = dir.path().join("out");

    let report = export_memory_dir(&store, &out).expect("export");

    assert_eq!(report.facts, 1, "{report:?}");
    assert!(
        out.join("wb-bank-clients/goose-table-locking.md").is_file(),
        "expected the fact at its namespace path"
    );
}

/// The frontmatter has to match what the corpus already uses, or the export is
/// a different format wearing the same extension.
#[test]
fn the_exported_frontmatter_matches_the_corpus_format() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = seeded_store(dir.path());
    let out = dir.path().join("out");
    export_memory_dir(&store, &out).expect("export");

    let text =
        std::fs::read_to_string(out.join("wb-bank-clients/goose-table-locking.md")).expect("read");

    assert!(text.starts_with("---\n"), "{text}");
    assert!(text.contains("name: goose-table-locking"), "{text}");
    assert!(
        text.contains("description: goose v3.26 locks with NewPostgresTableLocker"),
        "{text}"
    );
    assert!(text.contains("type: project"), "{text}");
    assert!(text.contains("DDL rights"), "the body must survive: {text}");
}

/// The corpus is navigated through these index files, so an export without one
/// is a directory of orphans.
#[test]
fn an_index_is_written_for_each_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = seeded_store(dir.path());
    let out = dir.path().join("out");
    export_memory_dir(&store, &out).expect("export");

    let index = std::fs::read_to_string(out.join("wb-bank-clients/MEMORY.md")).expect("read");

    assert!(index.contains("goose-table-locking.md"), "{index}");
    assert!(
        index.contains("goose v3.26 locks"),
        "the index must carry titles, not just names: {index}"
    );
}

#[test]
fn wikilinks_are_written_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = seeded_store(dir.path());
    let out = dir.path().join("out");
    export_memory_dir(&store, &out).expect("export");

    let text =
        std::fs::read_to_string(out.join("wb-bank-clients/goose-table-locking.md")).expect("read");

    assert!(
        text.contains("[[goose-migrate-arm-testbed]]"),
        "relations must survive the round trip: {text}"
    );
}

// --- the round trip --------------------------------------------------------

/// The property that matters: import, export, import again, and nothing has
/// been lost or duplicated.
#[test]
fn a_corpus_survives_a_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    write(
        &corpus.join("svc/retry-budget.md"),
        "---\n\
         name: retry-budget\n\
         description: The retry budget is configurable per client\n\
         metadata:\n  \
           type: project\n\
         ---\n\n\
         Read from config rather than hardcoded. Related: [[client-config]].\n",
    );
    write(
        &corpus.join("claude/how-to-commit.md"),
        "---\n\
         name: how-to-commit\n\
         description: The user makes the commits\n\
         metadata:\n  \
           type: feedback\n\
         ---\n\n\
         Never commit without being asked.\n",
    );

    let first = Store::open(&dir.path().join("first.db")).expect("open");
    import_memory_dir(&first, &corpus).expect("import");

    let exported = dir.path().join("exported");
    export_memory_dir(&first, &exported).expect("export");

    let second = Store::open(&dir.path().join("second.db")).expect("open");
    let reimport = import_memory_dir(&second, &exported).expect("reimport");

    assert_eq!(reimport.facts, 2, "a fact was lost: {reimport:?}");
    assert!(reimport.skipped.is_empty(), "{:?}", reimport.skipped);

    let recovered = second
        .recall(
            "retry-budget",
            &FactContext {
                namespace: Some("svc".into()),
                ..FactContext::default()
            },
        )
        .expect("recall")
        .expect("the fact must survive");

    assert_eq!(
        recovered.title, "The retry budget is configurable per client",
        "the title did not survive"
    );
    assert_eq!(recovered.kind, FactKind::Project);

    // Personal facts have no namespace, so they must land somewhere that
    // re-imports as user scope rather than becoming a project's memory.
    let personal = second
        .recall("how-to-commit", &FactContext::default())
        .expect("recall")
        .expect("the personal fact must survive");
    assert_eq!(personal.scope, FactScope::User);
}

/// Exporting twice must converge. Otherwise the corpus grows a duplicate on
/// every run and git shows churn that means nothing.
#[test]
fn exporting_twice_produces_the_same_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = seeded_store(dir.path());
    let out = dir.path().join("out");

    export_memory_dir(&store, &out).expect("first");
    let first =
        std::fs::read_to_string(out.join("wb-bank-clients/goose-table-locking.md")).expect("read");

    export_memory_dir(&store, &out).expect("second");
    let second =
        std::fs::read_to_string(out.join("wb-bank-clients/goose-table-locking.md")).expect("read");

    assert_eq!(first, second);
}
