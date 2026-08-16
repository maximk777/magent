//! Bringing the checkout Magent is served from up to date.
//!
//! The plugin is read live from the directory the marketplace points at, so
//! keeping Magent current means keeping that checkout current: pull, rebuild,
//! move the binary into place. Three commands in a directory that is not the
//! one you are working in — exactly the chore that quietly stops happening.
//!
//! The one thing this must never do is lose work. It runs `git pull` somewhere
//! a person may have edited, and a fast-forward that silently discarded a
//! change would be far worse than never updating at all. So it refuses on
//! anything uncommitted, refuses to merge a diverged branch, and never creates
//! a directory it was pointed at by mistake.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

/// Where this binary was built from.
///
/// Baked in at compile time, which is exactly right for the way Magent is
/// installed: from a checkout the marketplace also serves the plugin from. It
/// is a default rather than an assumption — `--from` overrides it, and the
/// directory is verified before anything is run in it.
fn built_from() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Runs the update, writing its report into `out`.
///
/// Returns false when nothing was done and the reason is a problem rather than
/// "already current".
pub fn run(from: Option<PathBuf>, build: bool, out: &mut String) -> bool {
    let source = from.unwrap_or_else(built_from);

    if !source.is_dir() {
        let _ = writeln!(out, "{} does not exist", source.display());
        return false;
    }
    if !is_magent_checkout(&source) {
        let _ = writeln!(
            out,
            "{} is not a Magent checkout — expected Cargo.toml, crates/ and plugin/",
            source.display()
        );
        return false;
    }

    // Before the fetch, not after: there is no point reaching the network to
    // discover that the pull could not have been applied anyway.
    if let Some(dirty) = uncommitted(&source) {
        let _ = writeln!(out, "{} has uncommitted changes:", source.display());
        let _ = writeln!(out, "{dirty}");
        let _ = writeln!(
            out,
            "commit or stash them first. Updating over them would lose work that\n\
             exists nowhere else."
        );
        return false;
    }

    let before = head(&source);

    match git(&source, &["pull", "--ff-only"]) {
        Ok(_) => {}
        Err(reason) => {
            let _ = writeln!(out, "could not update {}:", source.display());
            let _ = writeln!(out, "{reason}");
            if reason.contains("diverge") || reason.contains("fast-forward") {
                let _ = writeln!(
                    out,
                    "this checkout has commits upstream does not. Rebase or merge it\n\
                     yourself — deciding that on your behalf is not this command's job."
                );
            }
            return false;
        }
    }

    let after = head(&source);
    if before == after {
        let _ = writeln!(out, "already current at {}", short(&after));
        return true;
    }

    let _ = writeln!(out, "updated {} → {}", short(&before), short(&after));
    if let Ok(log) = git(
        &source,
        &["log", "--oneline", &format!("{before}..{after}")],
    ) {
        for line in log.lines() {
            let _ = writeln!(out, "  {line}");
        }
    }

    if !build {
        let _ = writeln!(
            out,
            "\nsources only. Run scripts/install.sh to rebuild the binary."
        );
        return true;
    }

    // install.sh rather than a rebuild here: it already writes the binary
    // beside the target and moves it, which is what stops macOS killing a
    // binary a running process has mapped, and it verifies the result runs.
    let _ = writeln!(out, "\nrebuilding...");
    match run_script(&source) {
        Ok(text) => {
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let _ = writeln!(out, "  {line}");
            }
            let _ = writeln!(
                out,
                "\nrestart Claude Code to pick up the new binary and any new skills."
            );
            true
        }
        Err(reason) => {
            let _ = writeln!(out, "the sources updated but the build failed:");
            let _ = writeln!(out, "{reason}");
            false
        }
    }
}

/// Whether this looks like the repository rather than some other checkout.
///
/// Checked before anything is run: `git pull` in a directory someone named by
/// mistake is a change to a repository they did not mean to touch.
fn is_magent_checkout(source: &Path) -> bool {
    source.join(".git").exists()
        && source.join("Cargo.toml").is_file()
        && source.join("crates").is_dir()
        && source.join("plugin").is_dir()
}

/// The uncommitted changes, if any, as porcelain lines.
fn uncommitted(source: &Path) -> Option<String> {
    let status = git(source, &["status", "--porcelain"]).ok()?;
    let trimmed = status.trim();
    (!trimmed.is_empty()).then(|| {
        trimmed
            .lines()
            .map(|line| format!("  {}", line.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn head(source: &Path) -> String {
    git(source, &["rev-parse", "HEAD"]).unwrap_or_default()
}

fn short(revision: &str) -> String {
    revision.chars().take(7).collect()
}

fn git(source: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(args)
        // Never prompt. A private remote without credentials would otherwise
        // hang a command someone ran and walked away from.
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| error.to_string())?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn run_script(source: &Path) -> Result<String, String> {
    let script = source.join("scripts/install.sh");
    if !script.is_file() {
        return Err(format!("{} is missing", script.display()));
    }

    let output = Command::new(&script)
        .current_dir(source)
        .output()
        .map_err(|error| error.to_string())?;

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    if output.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}
