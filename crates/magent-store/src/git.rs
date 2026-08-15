//! Read-only git probing.
//!
//! Magent observes repository state for context and handoff. It never writes to
//! a working tree: uncommitted work is unrecoverable, so cleaning or resetting
//! is not a capability this code has.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use magent_core::GitState;

/// What a working directory turned out to be.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryProbe {
    /// Canonical repository root, or the canonical directory itself when the
    /// path is not inside a repository.
    pub root: PathBuf,
    pub origin_url: Option<String>,
    pub git: Option<GitState>,
}

impl RepositoryProbe {
    /// Stable key a repository is stored under.
    ///
    /// Prefers the normalised origin so that clones, checkouts and linked
    /// worktrees of one project collapse into a single identity; falls back to
    /// the canonical path for local-only or non-git directories.
    #[must_use]
    pub fn identity_key(&self) -> String {
        self.origin_url.as_ref().map_or_else(
            || format!("path:{}", self.root.display()),
            |origin| format!("git:{}", normalize_origin(origin)),
        )
    }
}

/// Inspects `start`, resolving the repository it belongs to.
///
/// Never fails: a missing `git` binary, a path outside any repository, or a
/// repository in a broken state all degrade to a path identity rather than
/// breaking the caller's session.
#[must_use]
pub fn discover(start: &Path) -> RepositoryProbe {
    let canonical = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());

    let Some(toplevel) = run_git(&canonical, &["rev-parse", "--show-toplevel"]) else {
        return RepositoryProbe {
            root: canonical,
            origin_url: None,
            git: None,
        };
    };

    let root = std::fs::canonicalize(&toplevel).unwrap_or_else(|_| PathBuf::from(&toplevel));

    RepositoryProbe {
        origin_url: run_git(&root, &["remote", "get-url", "origin"]),
        git: state(&root),
        root,
    }
}

/// The repository root containing `path`, if any.
///
/// Used to render file paths relative in the restoration packet: an absolute
/// path repeated a dozen times is mostly one prefix, and the packet is paid for
/// in context on every session.
#[must_use]
pub fn toplevel(path: &Path) -> Option<PathBuf> {
    let raw = run_git(path, &["rev-parse", "--show-toplevel"])?;
    Some(std::fs::canonicalize(&raw).unwrap_or_else(|_| PathBuf::from(raw)))
}

/// Point-in-time git state for an already known repository root.
///
/// One subprocess, not four: `PreCompact` runs this on the critical path with a
/// 100 ms budget, and process spawns dominate that budget. `--porcelain=v2
/// --branch` reports HEAD, the branch and every change in a single pass.
#[must_use]
pub fn state(root: &Path) -> Option<GitState> {
    let output = run_git_raw(root, &["status", "--porcelain=v2", "--branch"])?;

    let mut branch = None;
    let mut sha = None;
    let mut dirty_files = 0_u32;

    for line in output.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            if let Some(value) = header.strip_prefix("branch.oid ") {
                // An unborn branch reports "(initial)" rather than an object id.
                sha = (value != "(initial)").then(|| value.to_owned());
            } else if let Some(value) = header.strip_prefix("branch.head ") {
                // A detached HEAD reports "(detached)", which is not a branch.
                branch = (value != "(detached)").then(|| value.to_owned());
            }
        } else if !line.trim().is_empty() {
            // Changed ("1"/"2"), unmerged ("u") and untracked ("?") entries all
            // count: a file the agent created but never committed is still work
            // that must not be lost.
            dirty_files = dirty_files.saturating_add(1);
        }
    }

    Some(GitState {
        branch,
        sha,
        dirty_files,
    })
}

/// Normalises an origin URL so its SSH and HTTPS forms agree.
///
/// `git@github.com:acme/thing.git` and `https://github.com/acme/thing` both
/// become `github.com/acme/thing`. Without this, cloning a project the other
/// way would hand it a second, empty memory.
///
/// The host is lowercased because DNS is case-insensitive; the path is not,
/// because on some forges it is significant.
#[must_use]
pub fn normalize_origin(origin: &str) -> String {
    let trimmed = origin.trim().trim_end_matches('/');

    // scp-like syntax: [user@]host:path
    let without_scheme = if let Some(rest) = strip_scheme(trimmed) {
        rest.to_owned()
    } else if let Some((prefix, path)) = trimmed.split_once(':')
        && !path.starts_with('/')
        && !prefix.contains('/')
    {
        let host = prefix.rsplit('@').next().unwrap_or(prefix);
        format!("{host}/{path}")
    } else {
        trimmed.to_owned()
    };

    let without_credentials = without_credentials(&without_scheme);
    let (host, path) = without_credentials
        .split_once('/')
        .unwrap_or((without_credentials.as_str(), ""));

    let host = host.to_lowercase();
    let path = path.trim_start_matches('/').trim_end_matches(".git");

    if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

fn strip_scheme(value: &str) -> Option<&str> {
    for scheme in ["https://", "http://", "ssh://", "git://", "file://"] {
        if let Some(rest) = value.strip_prefix(scheme) {
            return Some(rest);
        }
    }
    None
}

fn without_credentials(value: &str) -> String {
    match value.split_once('/') {
        Some((authority, rest)) => {
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{host}/{rest}")
        }
        None => value.rsplit('@').next().unwrap_or(value).to_owned(),
    }
}

fn run_git(directory: &Path, args: &[&str]) -> Option<String> {
    run_git_raw(directory, args).map(|value| value.trim().to_owned())
}

fn run_git_raw(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;

    if output.status.success() {
        String::from_utf8(output.stdout).ok()
    } else {
        None
    }
}
