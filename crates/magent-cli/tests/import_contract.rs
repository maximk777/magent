//! Importing existing memory.
//!
//! There is a real corpus to bring across: a hundred-odd markdown facts filed
//! per project, plus a Codex archive of session summaries. The importer has to
//! survive that corpus as it actually is — some files have no frontmatter, some
//! link to facts nobody ever wrote — because a strict importer that refuses the
//! awkward tenth is an importer nobody runs.

use std::path::Path;

use magent_cli::import::{import_codex_rollouts, import_memory_dir};
use magent_core::{FactKind, FactScope};
use magent_store::{FactContext, FactQuery, Store};

fn temp_store(dir: &Path) -> Store {
    Store::open(&dir.join("magent.db")).expect("open")
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

/// Mirrors the shape of the real corpus, including its awkward cases.
fn build_corpus(root: &Path) {
    write(
        &root.join("wbbank-wb-bank-clients/goose-table-locking.md"),
        "---\n\
         name: goose-table-locking\n\
         description: goose v3.26 locks with NewPostgresTableLocker\n\
         metadata:\n  \
           node_type: memory\n  \
           type: project\n  \
           originSessionId: 7a415757-0d4e-414a-a31b-da8abd1ced31\n\
         ---\n\n\
         Use `lock.NewPostgresTableLocker(...)` with `goose.WithLocker(locker)`.\n\n\
         **Why:** goose_lock is auto-created and needs DDL rights.\n\n\
         Related: [[goose-migrate-arm-testbed]].\n",
    );

    write(
        &root.join("wbbank-wb-bank-clients/MEMORY.md"),
        "- [goose table locking](goose-table-locking.md) — index line, not a fact\n",
    );

    // Three files in the real corpus have no frontmatter at all.
    write(
        &root.join("wbbank-wb-bank-clients/providers.md"),
        "# Providers\n\nThe provider set is wired in internal/di/providers.go.\n",
    );

    write(
        &root.join("claude/opencode-migration.md"),
        "---\n\
         name: opencode-migration\n\
         description: How the opencode migration was approached\n\
         metadata:\n  \
           type: feedback\n\
         ---\n\n\
         Keep the harness, port the config.\n",
    );
}

// --- the markdown corpus ---------------------------------------------------

#[test]
fn every_fact_in_the_corpus_is_imported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());

    let report = import_memory_dir(&store, &corpus).expect("import");

    assert_eq!(report.facts, 3, "skipped something: {report:?}");
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
}

/// An index is a table of contents, not knowledge. Importing it would create a
/// fact whose body is a list of links to other facts.
#[test]
fn index_files_are_not_facts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());

    import_memory_dir(&store, &corpus).expect("import");

    assert!(
        store
            .recall("memory", &FactContext::default())
            .expect("recall")
            .is_none(),
        "MEMORY.md was imported as a fact"
    );
}

#[test]
fn the_directory_becomes_the_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());
    import_memory_dir(&store, &corpus).expect("import");

    let found = store
        .search(&FactQuery {
            text: Some("DDL rights".into()),
            namespaces: vec!["wbbank-wb-bank-clients".into()],
            ..FactQuery::default()
        })
        .expect("search");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].namespace.as_deref(),
        Some("wbbank-wb-bank-clients")
    );
}

/// Frontmatter carries the taxonomy the corpus already uses. Flattening it into
/// "notes" would lose the distinction between how the user wants to work and
/// what is true of a codebase.
#[test]
fn the_frontmatter_taxonomy_is_preserved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());
    import_memory_dir(&store, &corpus).expect("import");

    let project = store
        .recall(
            "goose-table-locking",
            &FactContext {
                namespace: Some("wbbank-wb-bank-clients".into()),
                ..FactContext::default()
            },
        )
        .expect("recall")
        .expect("fact");
    assert_eq!(project.kind, FactKind::Project);
    assert_eq!(project.scope, FactScope::Repository);
}

/// `claude/` and `research/` are not projects; what is filed there is about the
/// user and applies wherever they work.
#[test]
fn the_personal_directories_import_as_user_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());
    import_memory_dir(&store, &corpus).expect("import");

    let found = store
        .search(&FactQuery {
            text: Some("opencode".into()),
            namespaces: vec!["an-unrelated-project".into()],
            ..FactQuery::default()
        })
        .expect("search");

    assert_eq!(found.len(), 1, "user-scoped memory must travel: {found:?}");
    assert_eq!(found[0].scope, FactScope::User);
}

/// A tenth of the corpus would be lost to a strict parser, and those files hold
/// real knowledge.
///
/// Its title comes from the opening sentence rather than the `# Providers`
/// heading: that heading is the slug again, and an index line that repeats the
/// name it sits next to conveys nothing.
#[test]
fn a_file_without_frontmatter_is_still_imported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());
    import_memory_dir(&store, &corpus).expect("import");

    let recovered = store
        .recall(
            "providers",
            &FactContext {
                namespace: Some("wbbank-wb-bank-clients".into()),
                ..FactContext::default()
            },
        )
        .expect("recall")
        .expect("the file must not be dropped");

    assert_eq!(
        recovered.title, "The provider set is wired in internal/di/providers.go",
        "a title that merely repeats the name is no title at all"
    );
}

/// Some files carry `title: <the-slug-again>` and put the real heading in the
/// body. Taking the frontmatter title at face value there yields an index full
/// of filenames, which tells the model nothing it did not already know.
#[test]
fn a_title_that_merely_repeats_the_name_loses_to_the_heading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    write(
        &corpus.join("research/bun-production-experience.md"),
        "---\n\
         title: bun-production-experience\n\
         type: note\n\
         ---\n\n\
         # Bun in production 2025-2026: what actually happened\n\n\
         Bun as a runtime is niche; bun install and bun test are a clear win.\n",
    );
    let store = temp_store(dir.path());
    import_memory_dir(&store, &corpus).expect("import");

    let fact = store
        .recall("bun-production-experience", &FactContext::default())
        .expect("recall")
        .expect("fact");

    assert_eq!(
        fact.title, "Bun in production 2025-2026: what actually happened",
        "the slug was used as the title instead of the heading"
    );
}

/// A handful of files have neither a description nor a heading. Falling back to
/// the slug leaves an index line that says nothing; the opening sentence at
/// least says what the file is about.
#[test]
fn a_file_with_no_title_at_all_falls_back_to_its_opening_sentence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    write(
        &corpus.join("svc/phase-five-redesign.md"),
        "---\n\
         title: phase-five-redesign\n\
         type: config\n\
         ---\n\n\
         The current phase is a full rebuild of the BFF on the client-card domain.\n\n\
         More detail follows here.\n",
    );
    let store = temp_store(dir.path());
    import_memory_dir(&store, &corpus).expect("import");

    let fact = store
        .recall(
            "phase-five-redesign",
            &FactContext {
                namespace: Some("svc".into()),
                ..FactContext::default()
            },
        )
        .expect("recall")
        .expect("fact");

    assert!(
        fact.title
            .starts_with("The current phase is a full rebuild"),
        "expected the opening sentence, got {:?}",
        fact.title
    );
}

#[test]
fn wikilinks_become_relations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());
    let report = import_memory_dir(&store, &corpus).expect("import");

    assert_eq!(report.relations, 1);

    let context = FactContext {
        namespace: Some("wbbank-wb-bank-clients".into()),
        ..FactContext::default()
    };
    let relations = store
        .relations_of("goose-table-locking", &context)
        .expect("relations");
    assert_eq!(relations[0].0, "goose-migrate-arm-testbed");
}

/// Importing twice is a normal thing to do: the corpus keeps being written to
/// while Magent is being adopted. It must converge, not accumulate.
#[test]
fn importing_twice_does_not_duplicate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("memory");
    build_corpus(&corpus);
    let store = temp_store(dir.path());

    import_memory_dir(&store, &corpus).expect("first");
    import_memory_dir(&store, &corpus).expect("second");

    let all = store
        .search(&FactQuery {
            namespaces: vec!["wbbank-wb-bank-clients".into()],
            limit: 100,
            ..FactQuery::default()
        })
        .expect("search");

    // By name, not by count: user-scoped facts are visible from every
    // namespace, so a raw total says nothing about duplication.
    let mut names: Vec<&str> = all.iter().map(|fact| fact.name.as_str()).collect();
    names.sort_unstable();
    let unique = {
        let mut deduped = names.clone();
        deduped.dedup();
        deduped
    };

    assert_eq!(names, unique, "a re-import duplicated facts: {names:?}");
}

// --- the Codex archive -----------------------------------------------------

/// Rollout summaries are records of past sessions. The plan called for
/// importing them as checkpoints, but a checkpoint needs a run and a session:
/// bringing a hundred of them across would have meant fabricating a hundred
/// runs that were never worked on. They land as reference facts instead,
/// attached to the project their cwd names.
#[test]
fn codex_rollouts_import_as_references_scoped_by_their_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rollouts = dir.path().join("rollout_summaries");
    write(
        &rollouts.join("2026-08-10T21-54-34-z9bH-magent-audit.md"),
        "thread_id: 019fedab-9bb7-7d73-8f62-b5fd13e02341\n\
         updated_at: 2026-08-11T19:40:09+00:00\n\
         cwd: /Users/someone/programming/wbbank/wb-bank-clients\n\
         git_branch: main\n\n\
         # Goose migration lock diagnosis\n\n\
         The advisory lock contention was separated from the probe budget.\n",
    );
    let store = temp_store(dir.path());

    let report = import_codex_rollouts(&store, &rollouts).expect("import");
    assert_eq!(report.facts, 1);

    let found = store
        .search(&FactQuery {
            text: Some("advisory lock".into()),
            namespaces: vec!["wb-bank-clients".into()],
            ..FactQuery::default()
        })
        .expect("search");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, FactKind::Reference);
}

#[test]
fn a_rollout_without_a_cwd_is_reported_rather_than_dropped_silently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rollouts = dir.path().join("rollout_summaries");
    write(
        &rollouts.join("headerless.md"),
        "# Something happened\n\nNo header at all.\n",
    );
    let store = temp_store(dir.path());

    let report = import_codex_rollouts(&store, &rollouts).expect("import");

    assert_eq!(report.facts, 0);
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(
        report.skipped[0].1.contains("cwd"),
        "the reason must say what was missing: {:?}",
        report.skipped[0]
    );
}

// --- robustness ------------------------------------------------------------

#[test]
fn importing_a_directory_that_does_not_exist_is_not_a_crash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = temp_store(dir.path());

    let report = import_memory_dir(&store, &dir.path().join("nope")).expect("import");
    assert_eq!(report.facts, 0);
}
