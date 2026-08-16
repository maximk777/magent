//! Pulling a newer Magent and putting it in place.
//!
//! The plugin is read live from the checkout it was installed from, so keeping
//! Magent current means keeping that checkout current. Doing it by hand is
//! three commands in a directory that is not the one you are working in, which
//! is exactly the kind of chore that quietly stops happening.
//!
//! What this must never do is lose work. It runs `git pull` in a directory
//! someone may have edited, and a fast-forward that silently discarded a change
//! would be far worse than never updating at all.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

const MAGENT: &str = env!("CARGO_BIN_EXE_magent");

fn git(directory: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct World {
    dir: tempfile::TempDir,
    upstream: PathBuf,
    checkout: PathBuf,
}

impl World {
    /// An upstream with one commit, and a clone of it that looks like a Magent
    /// checkout.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).expect("mkdir");

        git(&upstream, &["init", "-b", "main"]);
        git(&upstream, &["config", "user.email", "t@example.invalid"]);
        git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("Cargo.toml"), "[workspace]\n").expect("write");
        // With files in them: git does not track empty directories, so a
        // clone would arrive without the very layout the check looks for.
        std::fs::create_dir_all(upstream.join("crates/magent-cli/src")).expect("mkdir");
        std::fs::write(
            upstream.join("crates/magent-cli/src/main.rs"),
            "fn main() {}\n",
        )
        .expect("write");
        std::fs::create_dir_all(upstream.join("plugin")).expect("mkdir");
        std::fs::write(upstream.join("plugin/.mcp.json"), "{}\n").expect("write");
        std::fs::write(upstream.join("README.md"), "one\n").expect("write");
        git(&upstream, &["add", "-A"]);
        git(&upstream, &["commit", "-m", "first"]);

        let checkout = dir.path().join("checkout");
        let output = Command::new("git")
            .args(["clone", "-q"])
            .arg(&upstream)
            .arg(&checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("clone");
        assert!(output.status.success());
        git(&checkout, &["config", "user.email", "t@example.invalid"]);
        git(&checkout, &["config", "user.name", "T"]);

        Self {
            dir,
            upstream,
            checkout,
        }
    }

    fn commit_upstream(&self, contents: &str, message: &str) {
        std::fs::write(self.upstream.join("README.md"), contents).expect("write");
        git(&self.upstream, &["commit", "-am", message]);
    }

    /// `--no-build` throughout: these tests are about the pull and its safety,
    /// and a release build inside each one would make them useless to run.
    fn update(&self) -> (bool, String) {
        let output = Command::new(MAGENT)
            .args(["update", "--no-build", "--from"])
            .arg(&self.checkout)
            .env("MAGENT_STATE_DIR", self.dir.path().join("state"))
            .output()
            .expect("run magent update");

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    fn head_message(&self) -> String {
        git(&self.checkout, &["log", "-1", "--format=%s"])
            .trim()
            .to_owned()
    }
}

// --- the ordinary case ------------------------------------------------------

#[test]
fn a_newer_upstream_is_pulled_and_what_arrived_is_reported() {
    let world = World::new();
    world.commit_upstream("two\n", "the second thing");

    let (ok, report) = world.update();

    assert!(ok, "{report}");
    assert_eq!(world.head_message(), "the second thing");
    assert!(
        report.contains("the second thing"),
        "an update that does not say what arrived is a shrug: {report}"
    );
}

#[test]
fn an_already_current_checkout_says_so_and_changes_nothing() {
    let world = World::new();

    let (ok, report) = world.update();

    assert!(ok, "{report}");
    assert!(report.to_lowercase().contains("already"), "{report}");
    assert_eq!(world.head_message(), "first");
}

// --- refusing to lose work --------------------------------------------------

/// The failure worth designing against. Someone edits the checkout, forgets,
/// and runs update; a pull that discarded it would be unrecoverable and silent.
#[test]
fn uncommitted_work_stops_the_update() {
    let world = World::new();
    world.commit_upstream("two\n", "the second thing");
    std::fs::write(world.checkout.join("README.md"), "mine\n").expect("write");

    let (ok, report) = world.update();

    assert!(!ok, "it should refuse: {report}");
    assert_eq!(
        std::fs::read_to_string(world.checkout.join("README.md")).expect("read"),
        "mine\n",
        "the edit was lost"
    );
    assert_eq!(world.head_message(), "first", "it pulled anyway");
    assert!(
        report.contains("README.md"),
        "naming what is in the way is the whole point: {report}"
    );
}

/// A local commit that upstream does not have means a fast-forward is not
/// possible. Merging or rebasing on someone's behalf is not this command's
/// business.
#[test]
fn a_diverged_checkout_is_refused_rather_than_merged() {
    let world = World::new();
    world.commit_upstream("two\n", "upstream moved");
    std::fs::write(world.checkout.join("local.txt"), "mine\n").expect("write");
    git(&world.checkout, &["add", "-A"]);
    git(&world.checkout, &["commit", "-m", "local work"]);

    let (ok, report) = world.update();

    assert!(!ok, "{report}");
    assert_eq!(world.head_message(), "local work", "the local commit moved");
    assert!(
        report.to_lowercase().contains("diverged")
            || report.to_lowercase().contains("fast-forward"),
        "the reason has to be legible: {report}"
    );
}

#[test]
fn a_directory_that_is_not_a_magent_checkout_is_refused() {
    let world = World::new();
    let elsewhere = world.dir.path().join("not-magent");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    git(&elsewhere, &["init", "-b", "main"]);

    let output = Command::new(MAGENT)
        .args(["update", "--no-build", "--from"])
        .arg(&elsewhere)
        .env("MAGENT_STATE_DIR", world.dir.path().join("state"))
        .output()
        .expect("run");

    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("not a Magent checkout") || text.contains("magent"),
        "{text}"
    );
}

/// Being told to update a directory that does not exist is a typo, not a
/// reason to clone something.
#[test]
fn a_missing_directory_is_refused_rather_than_created() {
    let world = World::new();
    let missing = world.dir.path().join("nowhere");

    let output = Command::new(MAGENT)
        .args(["update", "--no-build", "--from"])
        .arg(&missing)
        .env("MAGENT_STATE_DIR", world.dir.path().join("state"))
        .output()
        .expect("run");

    assert!(!output.status.success());
    assert!(!missing.exists(), "it created the directory");
}
