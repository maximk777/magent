//! Storing, superseding and finding facts.
//!
//! Memory is only useful if a later contradiction is recorded rather than
//! silently overwriting what came before, and if retrieval brings back what is
//! relevant here without dragging in every project the user has ever touched.
//! Those two properties are what this file pins.

use magent_core::{
    Cardinality, Evidence, FactKind, FactScope, FactStatus, OperationId, RelationKind,
    RememberCommand,
};
use magent_store::{FactContext, FactQuery, Store, namespace_candidates};

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("magent.db")).expect("open");
    (dir, store)
}

fn remember(name: &str, title: &str, body: &str) -> RememberCommand {
    RememberCommand {
        operation_id: OperationId::new(),
        name: name.into(),
        title: title.into(),
        body: body.into(),
        kind: FactKind::Project,
        scope: FactScope::Repository,
        cardinality: Cardinality::Single,
        status: FactStatus::Observed,
        confidence: 0.7,
        evidence: vec![],
        relates_to: vec![],
    }
}

fn in_namespace(namespace: &str) -> FactContext {
    FactContext {
        namespace: Some(namespace.to_owned()),
        ..FactContext::default()
    }
}

fn query(text: &str, namespaces: &[&str]) -> FactQuery {
    FactQuery {
        text: Some(text.to_owned()),
        namespaces: namespaces.iter().map(|n| (*n).to_owned()).collect(),
        ..FactQuery::default()
    }
}

// --- storing and recalling -------------------------------------------------

#[test]
fn a_remembered_fact_comes_back_by_name() {
    let (_dir, store) = temp_store();
    store
        .remember(
            &remember(
                "goose-table-locking",
                "goose v3.26 locks with NewPostgresTableLocker",
                "Use lock.NewPostgresTableLocker plus goose.WithLocker.",
            ),
            &in_namespace("wb-bank-clients"),
        )
        .expect("remember");

    let recalled = store
        .recall("goose-table-locking", &in_namespace("wb-bank-clients"))
        .expect("recall")
        .expect("the fact exists");

    assert_eq!(
        recalled.title,
        "goose v3.26 locks with NewPostgresTableLocker"
    );
    assert!(recalled.body.contains("WithLocker"));
}

#[test]
fn recalling_something_never_written_is_not_an_error() {
    let (_dir, store) = temp_store();

    assert!(
        store
            .recall("never-written", &FactContext::default())
            .expect("recall")
            .is_none()
    );
}

// --- cardinality -----------------------------------------------------------

/// A single-valued fact has one current value. The old one is superseded, not
/// deleted: knowing what was believed before is how a wrong turn gets diagnosed.
#[test]
fn a_single_valued_fact_supersedes_its_predecessor() {
    let (_dir, store) = temp_store();
    let context = in_namespace("service");

    store
        .remember(
            &remember(
                "retry-budget",
                "retry budget is 3",
                "hardcoded in client.rs",
            ),
            &context,
        )
        .expect("first");
    store
        .remember(
            &remember(
                "retry-budget",
                "retry budget is configurable",
                "now read from config",
            ),
            &context,
        )
        .expect("second");

    let current = store
        .recall("retry-budget", &context)
        .expect("recall")
        .expect("a current value");
    assert_eq!(current.title, "retry budget is configurable");

    let history = store
        .fact_history("retry-budget", &context)
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "the earlier value must survive: {history:?}"
    );
    assert!(
        history.iter().any(
            |fact| fact.status == FactStatus::Contradicted || fact.title == "retry budget is 3"
        ),
        "the predecessor must still be readable"
    );
}

/// Set-valued facts are different values of the same subject, not competing
/// claims. Superseding them would throw away most of what was learned.
#[test]
fn set_valued_facts_coexist() {
    let (_dir, store) = temp_store();
    let context = in_namespace("service");

    for (name, title) in [
        ("known-flaky-a", "TestPaymentRetry is flaky"),
        ("known-flaky-b", "TestLedgerSync is flaky"),
    ] {
        store
            .remember(
                &RememberCommand {
                    cardinality: Cardinality::Set,
                    ..remember(name, title, "observed in CI")
                },
                &context,
            )
            .expect("remember");
    }

    let found = store.search(&query("flaky", &["service"])).expect("search");
    assert_eq!(found.len(), 2, "both values must survive: {found:?}");
}

// --- search ----------------------------------------------------------------

#[test]
fn search_finds_a_fact_by_words_in_its_body() {
    let (_dir, store) = temp_store();
    store
        .remember(
            &remember(
                "goose-table-locking",
                "migration locking",
                "goose_lock is auto-created and needs DDL rights",
            ),
            &in_namespace("service"),
        )
        .expect("remember");

    let found = store
        .search(&query("DDL rights", &["service"]))
        .expect("search");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].name, "goose-table-locking");
}

/// The whole point of scope. A fact about one service must not surface while
/// working on an unrelated one, or memory becomes noise.
#[test]
fn search_does_not_leak_facts_between_projects() {
    let (_dir, store) = temp_store();
    store
        .remember(
            &remember("alpha-quirk", "alpha has a quirk", "only true of alpha"),
            &in_namespace("alpha"),
        )
        .expect("remember");

    let from_beta = store.search(&query("quirk", &["beta"])).expect("search");
    assert!(
        from_beta.is_empty(),
        "leaked into another project: {from_beta:?}"
    );

    let from_alpha = store.search(&query("quirk", &["alpha"])).expect("search");
    assert_eq!(from_alpha.len(), 1);
}

/// User-level facts are about the person, so they apply wherever they are
/// working.
#[test]
fn user_scoped_facts_apply_everywhere() {
    let (_dir, store) = temp_store();
    store
        .remember(
            &RememberCommand {
                scope: FactScope::User,
                kind: FactKind::Feedback,
                ..remember(
                    "no-autonomous-commits",
                    "never commit without being asked",
                    "the user makes the commits",
                )
            },
            &FactContext::default(),
        )
        .expect("remember");

    let found = store
        .search(&query("commit", &["some-unrelated-project"]))
        .expect("search");

    assert_eq!(found.len(), 1, "{found:?}");
}

/// A withdrawn fact must stop being retrieved, or revoking it would achieve
/// nothing.
#[test]
fn revoked_facts_do_not_come_back() {
    let (_dir, store) = temp_store();
    let context = in_namespace("service");
    store
        .remember(
            &RememberCommand {
                status: FactStatus::Revoked,
                ..remember("old-advice", "do it the old way", "no longer true")
            },
            &context,
        )
        .expect("remember");

    assert!(
        store
            .search(&query("old way", &["service"]))
            .expect("search")
            .is_empty()
    );
}

#[test]
fn search_honours_its_limit() {
    let (_dir, store) = temp_store();
    let context = in_namespace("service");
    for index in 0..10 {
        store
            .remember(
                &RememberCommand {
                    cardinality: Cardinality::Set,
                    ..remember(
                        &format!("note-{index}"),
                        "a note about latency",
                        "latency matters",
                    )
                },
                &context,
            )
            .expect("remember");
    }

    let found = store
        .search(&FactQuery {
            limit: 3,
            ..query("latency", &["service"])
        })
        .expect("search");

    assert_eq!(found.len(), 3);
}

// --- the pushed index ------------------------------------------------------

/// The index is injected on every prompt, so it carries titles and names only.
/// Including bodies would make it cost as much as the search it replaces.
#[test]
fn the_index_carries_no_bodies() {
    let (_dir, store) = temp_store();
    store
        .remember(
            &remember(
                "goose-table-locking",
                "migration locking",
                "A LONG BODY THAT MUST NOT BE PUSHED INTO CONTEXT",
            ),
            &in_namespace("service"),
        )
        .expect("remember");

    let index = store
        .fact_index(&query("locking", &["service"]))
        .expect("index");

    assert_eq!(index.len(), 1);
    let rendered = serde_json::to_string(&index).expect("serialize");
    assert!(
        !rendered.contains("LONG BODY"),
        "the index leaked a body: {rendered}"
    );
    assert!(rendered.contains("goose-table-locking"), "{rendered}");
}

// --- relations -------------------------------------------------------------

/// A link to a fact that was never written down records something worth
/// knowing: that it is missing.
#[test]
fn a_relation_to_an_unknown_fact_is_kept() {
    let (_dir, store) = temp_store();
    let context = in_namespace("service");

    store
        .remember(
            &RememberCommand {
                relates_to: vec![("not-written-yet".into(), RelationKind::Related)],
                ..remember("known", "a known thing", "body")
            },
            &context,
        )
        .expect("remember");

    let relations = store.relations_of("known", &context).expect("relations");
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0].0, "not-written-yet");
}

// --- namespace binding -----------------------------------------------------

/// Imported memory is filed under a directory name, not a workspace id. The
/// candidates let a repository pick up its own history without anyone binding
/// it by hand.
#[test]
fn a_repository_path_yields_its_namespace_candidates() {
    let candidates = namespace_candidates(std::path::Path::new(
        "/Users/x/programming/wbbank/wb-bank-clients",
    ));

    assert!(
        candidates.contains(&"wb-bank-clients".to_owned()),
        "{candidates:?}"
    );
    assert!(
        candidates.contains(&"wbbank-wb-bank-clients".to_owned()),
        "the corpus files projects as <parent>-<repo>: {candidates:?}"
    );
}

#[test]
fn a_top_level_repository_still_yields_a_candidate() {
    let candidates = namespace_candidates(std::path::Path::new("/Users/x/hawk-eye"));
    assert!(
        candidates.contains(&"hawk-eye".to_owned()),
        "{candidates:?}"
    );
}

// --- evidence --------------------------------------------------------------

#[test]
fn evidence_survives_a_round_trip() {
    let (_dir, store) = temp_store();
    let context = in_namespace("service");

    store
        .remember(
            &RememberCommand {
                status: FactStatus::Verified,
                evidence: vec![Evidence {
                    locator: "internal/migrate/migrate.go:41".into(),
                    excerpt: Some("goose.WithLocker(locker)".into()),
                }],
                ..remember("locking", "verified locking", "checked in source")
            },
            &context,
        )
        .expect("remember");

    let recalled = store
        .recall("locking", &context)
        .expect("recall")
        .expect("fact");
    assert_eq!(recalled.evidence.len(), 1);
    assert_eq!(
        recalled.evidence[0].locator,
        "internal/migrate/migrate.go:41"
    );
}
