//! Curating memory by hand.
//!
//! Everything else writes memory automatically. This is the half a person does:
//! confirming what is true, withdrawing what is not, correcting wording, and
//! merging the duplicates that automation inevitably produces.
//!
//! Curation is the one place where a mistake is made deliberately, so nothing
//! here destroys anything. Every correction leaves what it replaced readable.

use magent_core::{
    Cardinality, Evidence, FactId, FactKind, FactScope, FactStatus, OperationId, RememberCommand,
};
use magent_store::{FactContext, FactQuery, Store};

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("magent.db")).expect("open");
    (dir, store)
}

fn context() -> FactContext {
    FactContext {
        namespace: Some("service".into()),
        ..FactContext::default()
    }
}

fn remember(store: &Store, name: &str, title: &str) -> FactId {
    store
        .remember(
            &RememberCommand {
                operation_id: OperationId::new(),
                name: name.into(),
                title: title.into(),
                body: format!("{title}, at length."),
                kind: FactKind::Project,
                scope: FactScope::Repository,
                cardinality: Cardinality::Set,
                status: FactStatus::Observed,
                confidence: 0.7,
                evidence: vec![],
                relates_to: vec![],
            },
            &context(),
        )
        .expect("remember")
}

fn visible(store: &Store) -> Vec<String> {
    store
        .search(&FactQuery {
            namespaces: vec!["service".into()],
            limit: 100,
            ..FactQuery::default()
        })
        .expect("search")
        .into_iter()
        .map(|fact| fact.name)
        .collect()
}

// --- confirming ------------------------------------------------------------

/// Confirming something is the point of curation, and evidence is what makes
/// the confirmation mean anything.
#[test]
fn a_fact_can_be_confirmed_with_evidence() {
    let (_dir, store) = temp_store();
    let id = remember(&store, "goose-locking", "goose locks with a table locker");

    store
        .verify_fact(
            id,
            &[Evidence {
                locator: "internal/migrate/migrate.go:41".into(),
                excerpt: Some("goose.WithLocker(locker)".into()),
            }],
        )
        .expect("verify");

    let fact = store
        .fact(id)
        .expect("read")
        .expect("the fact still exists");

    assert_eq!(fact.status, FactStatus::Verified);
    assert_eq!(fact.evidence.len(), 1);
    assert!(
        fact.confidence > 0.7,
        "confirming something should raise how far it is trusted, got {}",
        fact.confidence
    );
}

/// The strongest status must not be the cheapest to assert, by hand any more
/// than by tool.
#[test]
fn confirming_without_evidence_is_refused() {
    let (_dir, store) = temp_store();
    let id = remember(&store, "unchecked", "asserted without checking");

    assert!(
        store.verify_fact(id, &[]).is_err(),
        "a fact was marked verified with nothing behind it"
    );
    assert_eq!(
        store.fact(id).expect("read").expect("fact").status,
        FactStatus::Observed,
        "the refused change was applied anyway"
    );
}

// --- withdrawing -----------------------------------------------------------

#[test]
fn a_withdrawn_fact_stops_being_retrieved_but_stays_readable() {
    let (_dir, store) = temp_store();
    let id = remember(&store, "old-advice", "do it the old way");

    store
        .revoke_fact(id, "the API changed in v3")
        .expect("revoke");

    assert!(
        !visible(&store).contains(&"old-advice".to_owned()),
        "a withdrawn fact is still being retrieved"
    );

    let fact = store.fact(id).expect("read").expect("still readable");
    assert_eq!(fact.status, FactStatus::Revoked);
    assert!(
        fact.body.contains("the API changed in v3"),
        "the reason for withdrawing it was not kept: {}",
        fact.body
    );
}

/// Withdrawing is reversible: a person changing their mind is ordinary, and
/// making it permanent would make curation frightening.
#[test]
fn a_withdrawal_can_be_undone() {
    let (_dir, store) = temp_store();
    let id = remember(&store, "reinstated", "it was true after all");

    store
        .revoke_fact(id, "thought better of it")
        .expect("revoke");
    store
        .set_fact_status(id, FactStatus::Observed)
        .expect("reinstate");

    assert!(visible(&store).contains(&"reinstated".to_owned()));
}

// --- correcting ------------------------------------------------------------

/// A correction supersedes rather than overwrites, so what was believed before
/// stays available to explain a decision that was made on it.
#[test]
fn correcting_the_wording_keeps_the_previous_version() {
    let (_dir, store) = temp_store();
    let id = remember(&store, "retry-budget", "the retry budget is 3");

    let corrected = store
        .edit_fact(id, "the retry budget is configurable", "read from config")
        .expect("edit");

    assert_ne!(corrected, id, "an edit overwrote rather than superseded");

    let current = store
        .recall("retry-budget", &context())
        .expect("recall")
        .expect("a current value");
    assert_eq!(current.title, "the retry budget is configurable");

    let history = store
        .fact_history("retry-budget", &context())
        .expect("history");
    assert_eq!(history.len(), 2, "the previous wording was destroyed");
}

// --- promoting -------------------------------------------------------------

/// One fact at a time, unlike promoting a whole namespace: most of what a
/// person notices is general belongs to a group, not to the repository it
/// happened to be written in.
#[test]
fn a_single_fact_can_be_moved_to_the_workspace() {
    let (dir, store) = temp_store();
    let sibling = dir.path().join("sibling");
    std::fs::create_dir_all(&sibling).expect("mkdir");
    let workspace = store
        .group_into_workspace("group", std::slice::from_ref(&sibling))
        .expect("group");

    let id = remember(&store, "service-auth", "HMAC for clients");
    store
        .set_fact_scope(id, FactScope::Workspace, Some(workspace.workspace_id))
        .expect("promote");

    let from_sibling = store
        .search(&FactQuery {
            text: Some("HMAC clients".into()),
            namespaces: vec!["sibling".into()],
            workspace_id: Some(workspace.workspace_id),
            ..FactQuery::default()
        })
        .expect("search");

    assert_eq!(
        from_sibling.len(),
        1,
        "the promoted fact did not reach the group: {from_sibling:?}"
    );
}

// --- merging ---------------------------------------------------------------

/// Automation produces near-duplicates: the same thing learned twice, in two
/// sessions, worded differently. Merging is how the corpus stays readable.
#[test]
fn duplicates_can_be_merged_into_one() {
    let (_dir, store) = temp_store();
    let keep = remember(&store, "lock-timeout", "the lock timeout is 90s");
    let duplicate = remember(&store, "locking-timeout", "lock timeout: ninety seconds");

    store.merge_facts(keep, duplicate).expect("merge");

    let names = visible(&store);
    assert!(names.contains(&"lock-timeout".to_owned()));
    assert!(
        !names.contains(&"locking-timeout".to_owned()),
        "the duplicate is still being retrieved: {names:?}"
    );

    // Superseded rather than contradicted: the two said the same thing twice,
    // and recording a disagreement that never happened would mislead whoever
    // reads it later.
    let merged = store
        .fact(duplicate)
        .expect("read")
        .expect("the duplicate stays readable");
    assert_eq!(
        merged.title, "lock timeout: ninety seconds",
        "the folded-in wording was destroyed"
    );

    let relations = store
        .relations_of("lock-timeout", &context())
        .expect("relations");
    assert!(
        relations.iter().any(|(name, _)| name == "locking-timeout"),
        "the merge left no trace of what was folded in: {relations:?}"
    );
}

#[test]
fn a_fact_cannot_be_merged_into_itself() {
    let (_dir, store) = temp_store();
    let id = remember(&store, "only-one", "the only one");

    assert!(store.merge_facts(id, id).is_err());
    assert!(visible(&store).contains(&"only-one".to_owned()));
}

// --- finding duplicates ----------------------------------------------------

/// The console leads with these, so a loose heuristic is worse than none: a
/// list that is mostly noise teaches the reader to skip it.
///
/// Both pairs below are real, taken from the imported corpus.
#[test]
fn duplicate_detection_does_not_pair_merely_related_facts() {
    let (_dir, store) = temp_store();

    remember(
        &store,
        "project-expert-rewrite-plan",
        "Go-forward plan: freeze the current project-expert as RESEARCH and \
         redesign the system greenfield under a new name",
    );
    remember(
        &store,
        "wbbank-collab-prefs",
        "How the user wants me to work on project-expert: process, verification, \
         communication",
    );

    let pairs = store.duplicate_candidates(10).expect("candidates");

    assert!(
        pairs.is_empty(),
        "two facts about one project were called duplicates: {:?}",
        pairs
            .iter()
            .map(|(l, r)| (&l.name, &r.name))
            .collect::<Vec<_>>()
    );
}

#[test]
fn duplicate_detection_still_finds_the_same_thing_said_twice() {
    let (_dir, store) = temp_store();

    remember(
        &store,
        "goose-migration-lock-timeout",
        "the goose migration lock timeout was raised to ninety seconds",
    );
    remember(
        &store,
        "migration-lock-timeout-raised",
        "the migration lock timeout is now ninety seconds for goose",
    );

    let pairs = store.duplicate_candidates(10).expect("candidates");

    assert_eq!(
        pairs.len(),
        1,
        "the same thing said twice was not spotted: {:?}",
        pairs
            .iter()
            .map(|(l, r)| (&l.name, &r.name))
            .collect::<Vec<_>>()
    );
}

// --- what the console needs to show ----------------------------------------

/// The dashboard's numbers come from one place, so what it shows cannot drift
/// from what retrieval does.
#[test]
fn the_store_reports_what_a_console_needs() {
    let (_dir, store) = temp_store();
    remember(&store, "first", "the first thing");
    let revoked = remember(&store, "second", "the second thing");
    store
        .revoke_fact(revoked, "no longer true")
        .expect("revoke");

    let overview = store.overview().expect("overview");

    assert_eq!(
        overview.facts, 1,
        "revoked facts should not be counted live"
    );
    assert_eq!(overview.revoked, 1);
    assert_eq!(overview.namespaces, 1);
}

/// Browsing is not searching: a person opening the console wants to see what is
/// there, filtered, not to guess a query that reveals it.
#[test]
fn facts_can_be_browsed_by_scope_and_status() {
    let (_dir, store) = temp_store();
    remember(&store, "kept", "still true");
    let revoked = remember(&store, "dropped", "not any more");
    store.revoke_fact(revoked, "superseded").expect("revoke");

    let live = store
        .browse_facts(&magent_store::FactFilter {
            status: Some(FactStatus::Observed),
            ..magent_store::FactFilter::default()
        })
        .expect("browse");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].name, "kept");

    let withdrawn = store
        .browse_facts(&magent_store::FactFilter {
            status: Some(FactStatus::Revoked),
            ..magent_store::FactFilter::default()
        })
        .expect("browse");
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0].name, "dropped");
}

#[test]
fn browsing_can_be_narrowed_to_one_namespace() {
    let (_dir, store) = temp_store();
    remember(&store, "here", "in this project");
    store
        .remember(
            &RememberCommand {
                operation_id: OperationId::new(),
                name: "elsewhere".into(),
                title: "in another project".into(),
                body: "body".into(),
                kind: FactKind::Project,
                scope: FactScope::Repository,
                cardinality: Cardinality::Set,
                status: FactStatus::Observed,
                confidence: 0.7,
                evidence: vec![],
                relates_to: vec![],
            },
            &FactContext {
                namespace: Some("other".into()),
                ..FactContext::default()
            },
        )
        .expect("remember");

    let found = store
        .browse_facts(&magent_store::FactFilter {
            namespace: Some("service".into()),
            ..magent_store::FactFilter::default()
        })
        .expect("browse");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "here");
}
