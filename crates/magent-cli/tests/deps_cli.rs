//! Declaring reference checkouts from a terminal.
//!
//! Adding a dependency is a human decision — someone names a git URL — so the
//! terminal is the primary surface for it and the MCP tool only reads. The
//! contract that matters is that `magent deps` says where the sources landed,
//! because that path is the whole product: from there the agent's own grep and
//! read take over.

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
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
        std::fs::create_dir_all(&state_dir).expect("mkdir");

        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        Self {
            dir,
            state_dir,
            project,
        }
    }

    fn upstream(&self, name: &str, contents: &str) -> String {
        let root = self.dir.path().join("upstream").join(name);
        std::fs::create_dir_all(&root).expect("mkdir");
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.invalid"]);
        git(&root, &["config", "user.name", "T"]);
        std::fs::write(root.join("retry.go"), contents).expect("write");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "seed"]);
        format!("file://{}", root.display())
    }

    fn cli(&self, args: &[&str]) -> String {
        let output = Command::new(MAGENT)
            .args(args)
            .current_dir(&self.project)
            .env("MAGENT_STATE_DIR", &self.state_dir)
            .output()
            .expect("run magent");

        assert!(
            output.status.success(),
            "magent {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn cli_failing(&self, args: &[&str]) -> String {
        let output = Command::new(MAGENT)
            .args(args)
            .current_dir(&self.project)
            .env("MAGENT_STATE_DIR", &self.state_dir)
            .output()
            .expect("run magent");

        assert!(
            !output.status.success(),
            "magent {args:?} should have failed"
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

#[test]
fn adding_a_dependency_clones_it_and_says_where_it_is() {
    let world = World::new();
    let url = world.upstream("retry", "package retry\n");

    let added = world.cli(&["deps", "add", &url]);

    let checkout = added
        .lines()
        .find_map(|line| line.split_once("path: "))
        .map_or_else(
            || panic!("the path is the point of the command: {added}"),
            |(_, path)| PathBuf::from(path.trim()),
        );

    assert!(
        checkout.starts_with(world.state_dir.join("deps")),
        "checkouts live under the state directory: {}",
        checkout.display()
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("retry.go")).expect("the sources are readable"),
        "package retry\n"
    );
}

#[test]
fn listing_shows_what_is_declared_and_whether_it_is_on_disk() {
    let world = World::new();
    let url = world.upstream("retry", "package retry\n");
    world.cli(&["deps", "add", &url]);

    let listed = world.cli(&["deps", "list"]);

    assert!(listed.contains("present"), "{listed}");
    assert!(
        listed.contains("retry"),
        "the project has to be recognisable: {listed}"
    );
}

#[test]
fn nothing_declared_says_so_rather_than_printing_a_blank() {
    let world = World::new();
    let listed = world.cli(&["deps", "list"]);
    assert!(
        listed.to_lowercase().contains("no dependencies"),
        "{listed}"
    );
}

/// Syncing is the maintenance command: it is what a stale checkout needs, and
/// it must work across every declared dependency at once.
#[test]
fn syncing_updates_every_checkout() {
    let world = World::new();
    let url = world.upstream("retry", "before\n");
    let upstream_root = world.dir.path().join("upstream").join("retry");
    world.cli(&["deps", "add", &url]);

    std::fs::write(upstream_root.join("retry.go"), "after\n").expect("write");
    git(&upstream_root, &["commit", "-am", "second"]);

    world.cli(&["deps", "sync"]);

    let listed = world.cli(&["deps", "list"]);
    let checkout = listed
        .lines()
        .find_map(|line| line.split_once("path: "))
        .map_or_else(
            || panic!("list should show paths too: {listed}"),
            |(_, path)| PathBuf::from(path.trim()),
        );

    assert_eq!(
        std::fs::read_to_string(checkout.join("retry.go")).expect("read"),
        "after\n"
    );
}

/// A URL that cannot be reached must not look like a success, and must not
/// stop the command from having done its job on the others.
#[test]
fn an_unreachable_url_is_reported_without_failing_the_rest() {
    let world = World::new();
    let good = world.upstream("retry", "package retry\n");
    let missing = format!("file://{}", world.dir.path().join("nowhere").display());

    world.cli(&["deps", "add", &good]);
    let failure = world.cli_failing(&["deps", "add", &missing]);
    assert!(
        !failure.trim().is_empty(),
        "the reason has to reach the person who typed the URL"
    );

    let listed = world.cli(&["deps", "list"]);
    assert!(listed.contains("failed"), "{listed}");
    assert!(
        listed.contains("present"),
        "the good one still is: {listed}"
    );

    // And syncing keeps going past the broken one.
    world.cli(&["deps", "sync"]);
}

#[test]
fn removing_a_dependency_takes_its_sources_with_it() {
    let world = World::new();
    let url = world.upstream("retry", "package retry\n");
    let added = world.cli(&["deps", "add", &url]);
    let checkout = added
        .lines()
        .find_map(|line| line.split_once("path: "))
        .map(|(_, path)| PathBuf::from(path.trim()))
        .expect("path");

    world.cli(&["deps", "remove", &url]);

    assert!(!checkout.exists(), "the sources go with the declaration");
    let listed = world.cli(&["deps", "list"]);
    assert!(
        listed.to_lowercase().contains("no dependencies"),
        "{listed}"
    );
}
