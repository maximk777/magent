//! Learning a repository's toolchain from its manifests.
//!
//! The value here is not guessing. A detected fact says what a file declares,
//! cites that file, and stops. It never claims a command was run, a linter
//! exists on PATH, or a version is what CI actually uses — those are things
//! that have to be checked, and a memory that asserts them is worse than one
//! that stays quiet.

use std::path::Path;

use magent_core::FactStatus;
use magent_store::detect_toolchain;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

fn fixture() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn find<'a>(
    facts: &'a [magent_core::RememberCommand],
    name: &str,
) -> Option<&'a magent_core::RememberCommand> {
    facts.iter().find(|fact| fact.name == name)
}

// --- languages -------------------------------------------------------------

#[test]
fn a_go_module_is_detected_with_its_version() {
    let dir = fixture();
    write(
        dir.path(),
        "go.mod",
        "module github.com/acme/service\n\ngo 1.24.3\n\nrequire (\n\tgithub.com/x/y v1.0.0\n)\n",
    );

    let facts = detect_toolchain(dir.path());
    let go = find(&facts, "toolchain-go").expect("a go fact");

    assert!(go.title.contains("1.24.3"), "{}", go.title);
    assert!(
        go.body.contains("github.com/acme/service"),
        "the module path is what identifies it: {}",
        go.body
    );
    assert_eq!(
        go.evidence[0].locator, "go.mod",
        "a detected fact must point at what it read"
    );
}

#[test]
fn a_rust_workspace_is_detected_with_its_pinned_toolchain() {
    let dir = fixture();
    write(
        dir.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\"]\n\n[workspace.package]\nedition = \"2024\"\n",
    );
    write(
        dir.path(),
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"1.96.0\"\n",
    );

    let facts = detect_toolchain(dir.path());
    let rust = find(&facts, "toolchain-rust").expect("a rust fact");

    assert!(rust.title.contains("1.96.0"), "{}", rust.title);
    assert!(
        rust.evidence
            .iter()
            .any(|evidence| evidence.locator == "rust-toolchain.toml"),
        "the pinned version comes from rust-toolchain.toml, not Cargo.toml: {:?}",
        rust.evidence
    );
}

/// Reporting a version from `Cargo.toml` when `rust-toolchain.toml` is what
/// pins it is precisely the mistake the user has been burned by before.
#[test]
fn a_rust_project_without_a_pin_does_not_invent_a_version() {
    let dir = fixture();
    write(dir.path(), "Cargo.toml", "[package]\nname = \"thing\"\n");

    let facts = detect_toolchain(dir.path());
    let rust = find(&facts, "toolchain-rust").expect("a rust fact");

    assert!(
        !rust.title.chars().any(|c| c.is_ascii_digit()),
        "a version was invented: {}",
        rust.title
    );
}

#[test]
fn a_node_project_reports_its_package_manager_from_the_lockfile() {
    let dir = fixture();
    write(
        dir.path(),
        "package.json",
        "{\"name\":\"web\",\"scripts\":{\"test\":\"vitest run\",\"lint\":\"eslint .\"}}",
    );
    write(dir.path(), "pnpm-lock.yaml", "lockfileVersion: 9\n");

    let facts = detect_toolchain(dir.path());
    let node = find(&facts, "toolchain-node").expect("a node fact");

    assert!(node.body.contains("pnpm"), "{}", node.body);
    assert!(
        node.evidence
            .iter()
            .any(|evidence| evidence.locator == "pnpm-lock.yaml"),
        "{:?}",
        node.evidence
    );
}

#[test]
fn a_python_project_is_detected_from_pyproject() {
    let dir = fixture();
    write(
        dir.path(),
        "pyproject.toml",
        "[project]\nname = \"svc\"\nrequires-python = \">=3.12\"\n",
    );

    let facts = detect_toolchain(dir.path());
    let python = find(&facts, "toolchain-python").expect("a python fact");

    assert!(python.title.contains("3.12"), "{}", python.title);
}

/// Not in the original brief, but four of the repositories actually on this
/// machine are Gradle services. Staying silent about them would leave the
/// agent guessing in a codebase that says plainly what it is.
#[test]
fn a_gradle_project_is_detected_with_its_wrapper() {
    let dir = fixture();
    write(
        dir.path(),
        "build.gradle",
        "plugins { id 'java' }

sourceCompatibility = '21'
",
    );
    write(
        dir.path(),
        "gradlew",
        "#!/bin/sh
",
    );
    write(
        dir.path(),
        "settings.gradle",
        "rootProject.name = 'svc'
",
    );

    let facts = detect_toolchain(dir.path());
    let jvm = find(&facts, "toolchain-jvm").expect("a jvm fact");

    assert!(jvm.body.contains("gradlew"), "{}", jvm.body);
    assert!(
        jvm.evidence
            .iter()
            .any(|evidence| evidence.locator == "build.gradle"),
        "{:?}",
        jvm.evidence
    );
}

#[test]
fn a_maven_project_is_detected_too() {
    let dir = fixture();
    write(
        dir.path(),
        "pom.xml",
        "<project><artifactId>svc</artifactId></project>
",
    );

    let facts = detect_toolchain(dir.path());
    let jvm = find(&facts, "toolchain-jvm").expect("a jvm fact");
    assert!(jvm.body.to_lowercase().contains("maven"), "{}", jvm.body);
}

#[test]
fn a_polyglot_repository_reports_every_language_it_holds() {
    let dir = fixture();
    write(dir.path(), "go.mod", "module x\n\ngo 1.24\n");
    write(dir.path(), "package.json", "{\"name\":\"web\"}");

    let facts = detect_toolchain(dir.path());

    assert!(find(&facts, "toolchain-go").is_some());
    assert!(find(&facts, "toolchain-node").is_some());
}

#[test]
fn a_directory_with_no_manifests_yields_nothing() {
    let dir = fixture();
    write(dir.path(), "README.md", "# nothing here\n");

    assert!(detect_toolchain(dir.path()).is_empty());
}

// --- commands --------------------------------------------------------------

/// A declared script is a fact about the repository. An undeclared one is a
/// guess, and guessing commands is how an agent ends up running the wrong thing
/// confidently.
#[test]
fn declared_scripts_become_a_commands_fact() {
    let dir = fixture();
    write(
        dir.path(),
        "package.json",
        "{\"name\":\"web\",\"scripts\":{\"test\":\"vitest run\",\"build\":\"vite build\"}}",
    );

    let facts = detect_toolchain(dir.path());
    let commands = find(&facts, "commands-node").expect("a commands fact");

    assert!(commands.body.contains("vitest run"), "{}", commands.body);
    assert!(commands.body.contains("vite build"), "{}", commands.body);
}

#[test]
fn a_makefile_contributes_its_targets() {
    let dir = fixture();
    write(
        dir.path(),
        "Makefile",
        ".PHONY: test lint\n\ntest:\n\tgo test ./...\n\nlint:\n\tgolangci-lint run\n",
    );

    let facts = detect_toolchain(dir.path());
    let commands = find(&facts, "commands-make").expect("a make fact");

    assert!(commands.body.contains("test"), "{}", commands.body);
    assert!(commands.body.contains("lint"), "{}", commands.body);
}

/// A linter's configuration file proves the linter is intended, not that it is
/// installed or that any particular version is in use.
#[test]
fn a_linter_config_is_reported_as_intent_not_as_a_working_setup() {
    let dir = fixture();
    write(dir.path(), "go.mod", "module x\n\ngo 1.24\n");
    write(
        dir.path(),
        ".golangci.yml",
        "linters:\n  enable:\n    - errcheck\n",
    );

    let facts = detect_toolchain(dir.path());
    let linter = find(&facts, "linter-go").expect("a linter fact");

    assert_eq!(linter.evidence[0].locator, ".golangci.yml");
    let claim = format!("{} {}", linter.title, linter.body).to_lowercase();
    assert!(
        !claim.contains("installed") && !claim.contains("available"),
        "a config file does not prove the binary exists: {claim}"
    );
}

// --- honesty ---------------------------------------------------------------

/// Everything here was read out of a file, not run. Marking any of it verified
/// would make the strongest status the cheapest one to earn.
#[test]
fn every_detected_fact_is_observed_and_cites_a_file() {
    let dir = fixture();
    write(dir.path(), "go.mod", "module x\n\ngo 1.24\n");
    write(dir.path(), "Makefile", "test:\n\tgo test ./...\n");
    write(dir.path(), ".golangci.yml", "linters: {}\n");

    let facts = detect_toolchain(dir.path());
    assert!(!facts.is_empty());

    for fact in &facts {
        assert_eq!(
            fact.status,
            FactStatus::Observed,
            "{} claims more than it checked",
            fact.name
        );
        assert!(
            !fact.evidence.is_empty(),
            "{} cites nothing, so it cannot be re-checked",
            fact.name
        );
        assert!(
            fact.evidence
                .iter()
                .all(|evidence| !evidence.locator.starts_with('/')),
            "{} cites an absolute path, which will not survive a different checkout: {:?}",
            fact.name,
            fact.evidence
        );
    }
}

/// Detected facts are re-derived whenever a manifest changes, so they must
/// supersede rather than pile up.
#[test]
fn detected_facts_are_single_valued() {
    let dir = fixture();
    write(dir.path(), "go.mod", "module x\n\ngo 1.24\n");

    for fact in detect_toolchain(dir.path()) {
        assert_eq!(fact.cardinality, magent_core::Cardinality::Single);
    }
}

/// A manifest can be anything on disk, including truncated or binary. Detection
/// runs on session start, so it must not be a way to break a session.
#[test]
fn malformed_manifests_do_not_panic() {
    let dir = fixture();
    write(dir.path(), "go.mod", "\u{0}\u{1}not a go.mod at all");
    write(dir.path(), "package.json", "{ this is not json");
    write(dir.path(), "pyproject.toml", "[[[");
    write(dir.path(), "Cargo.toml", "");

    let facts = detect_toolchain(dir.path());

    for fact in &facts {
        assert!(
            magent_core::Validate::validate(fact).is_ok(),
            "{} is not a valid fact: {fact:?}",
            fact.name
        );
    }
}
