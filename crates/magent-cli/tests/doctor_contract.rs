//! What `magent doctor` has to answer.
//!
//! The question behind it is always the same: something is not working, or
//! might not be, and the person cannot tell whether the cause is Magent, the
//! toolchain, or their own change. A diagnostic that reports only what is fine
//! answers none of that, so this one leads with what is missing.
//!
//! It also has to be safe to run when things are broken. A doctor that needs a
//! healthy store to tell you the store is unhealthy is useless exactly when it
//! is wanted.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
}

struct World {
    dir: tempfile::TempDir,
    state_dir: PathBuf,
    project: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("state");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&state_dir).expect("mkdir");
        std::fs::create_dir_all(&project).expect("mkdir");

        git(&project, &["init", "-b", "main"]);
        git(&project, &["config", "user.email", "t@example.invalid"]);
        git(&project, &["config", "user.name", "T"]);

        Self {
            dir,
            state_dir,
            project,
        }
    }

    fn declare(&self, file: &str, contents: &str) {
        std::fs::write(self.project.join(file), contents).expect("write");
    }

    /// Runs doctor with a `PATH` this test controls, so "installed" means what
    /// the test says rather than what the machine happens to have.
    fn doctor(&self, path: &str) -> (bool, String) {
        let output = Command::new(MAGENT)
            .args(["doctor"])
            .current_dir(&self.project)
            .env("MAGENT_STATE_DIR", &self.state_dir)
            .env("PATH", path)
            .output()
            .expect("run magent doctor");

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    /// A directory holding fake executables, so a test can say what is on PATH.
    fn with_tools(&self, names: &[&str]) -> String {
        let bin = self.dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        for name in names {
            let path = bin.join(name);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        format!("{}:/usr/bin:/bin", bin.display())
    }
}

// --- the toolchain and what it needs ----------------------------------------

#[test]
fn a_missing_language_server_is_named_along_with_how_to_get_it() {
    let world = World::new();
    world.declare("go.mod", "module example.com/thing\n\ngo 1.24\n");

    let (ok, report) = world.doctor(&world.with_tools(&[]));

    assert!(ok, "a missing server is a finding, not a failure: {report}");
    assert!(report.contains("gopls"), "{report}");
    assert!(
        report.contains("go install") || report.contains("golang.org"),
        "naming what is missing without saying how to get it wastes the finding: {report}"
    );
}

#[test]
fn an_installed_server_is_reported_as_present() {
    let world = World::new();
    world.declare("go.mod", "module example.com/thing\n\ngo 1.24\n");

    let (_, report) = world.doctor(&world.with_tools(&["gopls"]));

    assert!(report.contains("gopls"), "{report}");
    assert!(
        !report.contains("go install"),
        "an install hint for something already installed is noise: {report}"
    );
}

/// Only what this repository actually uses. A Go project does not want to hear
/// about pyright, and a report full of irrelevant misses trains people to skip
/// it.
#[test]
fn servers_for_languages_this_repository_does_not_use_are_not_mentioned() {
    let world = World::new();
    world.declare("go.mod", "module example.com/thing\n\ngo 1.24\n");

    let (_, report) = world.doctor(&world.with_tools(&[]));

    assert!(!report.contains("pyright"), "{report}");
    assert!(!report.contains("rust-analyzer"), "{report}");
}

#[test]
fn several_toolchains_are_all_reported() {
    let world = World::new();
    world.declare("go.mod", "module example.com/thing\n\ngo 1.24\n");
    world.declare(
        "Cargo.toml",
        "[package]\nname = \"thing\"\nversion = \"0.1.0\"\n",
    );
    world.declare("package.json", "{\"name\":\"thing\"}\n");

    let (_, report) = world.doctor(&world.with_tools(&["gopls"]));

    for expected in ["gopls", "rust-analyzer", "typescript-language-server"] {
        assert!(
            report.contains(expected),
            "{expected} missing from: {report}"
        );
    }
}

// --- the state it reports on ------------------------------------------------

#[test]
fn the_report_says_which_profile_it_opened() {
    let world = World::new();
    let (_, report) = world.doctor(&world.with_tools(&[]));

    assert!(
        report.contains(&world.state_dir.display().to_string()),
        "a diagnostic that hides which database it read is unusable when there \
         are two: {report}"
    );
}

/// The failure this exists to catch. A reinstall that leaves an unrunnable
/// binary kills every hook silently, and the person sees Magent doing nothing
/// rather than an error.
#[test]
fn a_healthy_run_of_itself_is_confirmed() {
    let world = World::new();
    let (ok, report) = world.doctor(&world.with_tools(&[]));

    assert!(ok);
    assert!(
        report.contains("schema"),
        "the schema version is the first thing that explains a mismatch: {report}"
    );
}

/// Diagnostics have to survive the thing they diagnose.
#[test]
fn a_corrupt_store_is_reported_rather_than_crashed_on() {
    let world = World::new();
    std::fs::write(world.state_dir.join("magent.db"), b"this is not a database").expect("write");

    let (ok, report) = world.doctor(&world.with_tools(&[]));

    assert!(!ok, "a store that cannot be opened is a real failure");
    assert!(
        !report.trim().is_empty() && !report.contains("panicked"),
        "it should explain, not panic: {report}"
    );
}

/// Found by running this in a real repository: it was a Gradle project, the
/// JVM was detected, and the toolchain section printed a heading with nothing
/// under it. A heading with no rows reads as "nothing found" — the opposite of
/// what happened.
#[test]
fn a_toolchain_with_no_server_on_offer_is_still_reported() {
    let world = World::new();
    world.declare("build.gradle", "plugins { id 'java' }\n");

    let (_, report) = world.doctor(&world.with_tools(&[]));

    assert!(
        report.contains("JVM"),
        "the repository's toolchain was detected and then hidden: {report}"
    );
    assert!(
        report.to_lowercase().contains("no language server"),
        "saying nothing about why leaves it looking broken: {report}"
    );
}

/// And a repository that declares nothing prints no heading at all.
#[test]
fn a_repository_that_declares_nothing_gets_no_toolchain_section() {
    let world = World::new();
    let (_, report) = world.doctor(&world.with_tools(&[]));
    assert!(!report.contains("toolchain"), "{report}");
}

// --- the workflow this plugin ships ----------------------------------------

/// The SDD skills call the `openspec` CLI for every artifact they produce. A
/// plugin that ships skills depending on a binary and never mentions it leaves
/// the person to discover the dependency from a command that failed.
#[test]
fn a_missing_openspec_is_named_with_how_to_install_it() {
    let world = World::new();

    let (ok, report) = world.doctor(&world.with_tools(&[]));

    assert!(ok, "not having it is a finding, not a failure: {report}");
    assert!(report.contains("openspec"), "{report}");
    assert!(
        report.contains("@fission-ai/openspec"),
        "the package name is the useful half: {report}"
    );
}

#[test]
fn an_installed_openspec_is_not_nagged_about() {
    let world = World::new();

    let (_, report) = world.doctor(&world.with_tools(&["openspec"]));

    assert!(
        !report.contains("@fission-ai/openspec"),
        "an install line for something installed is noise: {report}"
    );
}

/// Having the CLI is not the same as this repository using it. Both are worth
/// knowing and they fail differently: one is "install something", the other is
/// "run openspec init here".
#[test]
fn a_repository_without_openspec_initialised_is_told_how_to_start() {
    let world = World::new();

    let (_, report) = world.doctor(&world.with_tools(&["openspec"]));

    assert!(
        report.contains("openspec init"),
        "nothing said about starting one here: {report}"
    );
    // A bare `openspec init` fails with a wall of thirty tool names. A hint
    // that does not work is worse than none, because it is followed.
    assert!(
        report.contains("--tools claude"),
        "the hint as printed would fail: {report}"
    );
}

#[test]
fn an_initialised_repository_is_left_alone() {
    let world = World::new();
    std::fs::create_dir_all(world.project.join("openspec/changes")).expect("mkdir");
    std::fs::write(
        world.project.join("openspec/config.yaml"),
        "schema: default\n",
    )
    .expect("write");

    let (_, report) = world.doctor(&world.with_tools(&["openspec"]));

    assert!(!report.contains("openspec init"), "{report}");
}
