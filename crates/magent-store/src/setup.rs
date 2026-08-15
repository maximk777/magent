//! Noticing what has not been set up, and proposing it.
//!
//! Magent registers a repository the first time a session opens in it, so
//! nothing has to be configured before it works. That is deliberate, but it
//! leaves one thing undone that matters more than anything else it does
//! automatically: grouping.
//!
//! Fifty checkouts of one organisation side by side are not fifty projects.
//! What is learned about their deploy pipeline or their service-to-service
//! authentication is true of all of them, and left ungrouped it is filed under
//! whichever directory happened to be open and never reaches the other
//! forty-nine. Grouping cannot be inferred safely enough to do silently — a
//! parent directory is also where unrelated work lives — so it is proposed and
//! confirmed instead.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    error::StoreError,
    git::{self, normalize_origin},
    store::Store,
};

/// A repository the proposal would sweep in.
#[derive(Clone, Debug, Serialize)]
pub struct Sibling {
    pub root: PathBuf,
    pub origin_url: Option<String>,
}

/// What setup found, and what it would do about it.
#[derive(Clone, Debug, Serialize)]
pub struct GroupingProposal {
    /// The repository the question was asked from.
    pub root: PathBuf,
    /// The organisation these checkouts share, `github.com/wbbank`.
    pub organisation: Option<String>,
    /// What the group would be called. `None` when there is nothing to propose.
    pub suggested_name: Option<String>,
    /// Everything the group would contain, including [`Self::root`].
    pub siblings: Vec<Sibling>,
    /// True when these already share a named workspace.
    pub already_grouped: bool,
    pub workspace_name: Option<String>,
}

/// How many checkouts it takes to be worth asking about.
///
/// Two: one repository is not a group, and offering to make one out of it
/// spends a decision on nothing.
const MIN_GROUP: usize = 2;

impl Store {
    /// Looks around `path` for checkouts that belong together.
    ///
    /// Read-only in every sense — an agent that merely looked must not have
    /// decided anything.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn propose_grouping(&self, path: &Path) -> Result<GroupingProposal, StoreError> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.propose_grouping_from(path, home.as_deref())
    }

    /// As [`Store::propose_grouping`], with the home directory given rather
    /// than read from the environment.
    ///
    /// Separated so the home-directory rule can be tested without a test
    /// reaching into a process-global that its neighbours share.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn propose_grouping_from(
        &self,
        path: &Path,
        home: Option<&Path>,
    ) -> Result<GroupingProposal, StoreError> {
        let probe = git::discover(path);
        let root = probe.root.clone();

        let Some(organisation) = probe.origin_url.as_deref().and_then(organisation_of) else {
            // No origin means nothing to group on. A directory's name says
            // where it sits, not who it belongs to.
            return Ok(GroupingProposal {
                root,
                organisation: None,
                suggested_name: None,
                siblings: Vec::new(),
                already_grouped: false,
                workspace_name: None,
            });
        };

        let siblings = siblings_sharing(&root, &organisation);
        let (already_grouped, workspace_name) = self.grouping_state(&root)?;

        // Everything found is still reported — seeing the siblings is useful
        // even where grouping them is not — but no name is offered, because a
        // suggestion is a recommendation and this would not be one.
        let suggested_name = (siblings.len() >= MIN_GROUP && !is_loose_in_home(&root, home))
            .then(|| name_for(&root, &organisation));

        Ok(GroupingProposal {
            root,
            organisation: Some(organisation),
            suggested_name,
            siblings,
            already_grouped,
            workspace_name,
        })
    }

    /// Whether `root` already sits in a workspace someone named.
    fn grouping_state(&self, root: &Path) -> Result<(bool, Option<String>), StoreError> {
        let connection = self.lock()?;
        let found: Option<(i64, String)> = connection
            .query_row(
                "SELECT w.explicit, w.name FROM repositories r
                 JOIN workspaces w ON w.id = r.workspace_id
                 WHERE r.canonical_root = ?1",
                [root.display().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        Ok(match found {
            Some((1, name)) => (true, Some(name)),
            _ => (false, None),
        })
    }
}

/// Whether these checkouts merely share the home directory.
///
/// Found by running this against a real machine: repositories cloned straight
/// into `$HOME` shared a parent and an account, and the proposal offered to
/// group them under the person's own name. That is not a project — `$HOME` is
/// where everything lives — and the group would have put unrelated work into
/// one memory.
fn is_loose_in_home(root: &Path, home: Option<&Path>) -> bool {
    let (Some(home), Some(parent)) = (home, root.parent()) else {
        return false;
    };

    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(parent) == canonical(home)
}

/// What to call the group.
///
/// The directory holding the checkouts, when it is a name rather than a place
/// to keep things. Run against a real corpus the organisation was `fintech`
/// while the person had already put everything under `wbbank` — and `wbbank`
/// is what they called the workspace when they grouped it by hand. A directory
/// name is a choice someone made; an organisation is an accident of hosting.
///
/// Only a default either way: the confirmation step lets it be edited, and the
/// organisation stays on the proposal as the alternative to offer.
fn name_for(root: &Path, organisation: &str) -> String {
    // Where code is kept, not what it is called. Suggesting one of these would
    // be worse than suggesting nothing, because it looks deliberate.
    const CONTAINERS: [&str; 12] = [
        "code",
        "src",
        "source",
        "projects",
        "project",
        "programming",
        "repos",
        "repositories",
        "git",
        "work",
        "workspace",
        "dev",
    ];

    let fallback = || {
        organisation
            .rsplit('/')
            .next()
            .unwrap_or(organisation)
            .to_owned()
    };

    let Some(parent) = root.parent().and_then(Path::file_name) else {
        return fallback();
    };
    let parent = parent.to_string_lossy().to_lowercase();

    if CONTAINERS.contains(&parent.as_str()) {
        fallback()
    } else {
        parent
    }
}

/// The host and organisation part of an origin: `github.com/wbbank`.
///
/// Deliberately not the host alone. Two projects on github.com have nothing in
/// common, and grouping on that would put the whole world in one workspace.
fn organisation_of(origin: &str) -> Option<String> {
    let normalised = normalize_origin(origin);
    let mut parts = normalised.splitn(3, '/');
    let host = parts.next()?;
    let organisation = parts.next()?;

    (!host.is_empty() && !organisation.is_empty()).then(|| format!("{host}/{organisation}"))
}

/// Checkouts beside `root` that share `organisation`, including `root` itself.
///
/// One level only. Walking deeper would find vendored copies and build output,
/// and the case this exists for is a flat directory of checkouts.
fn siblings_sharing(root: &Path, organisation: &str) -> Vec<Sibling> {
    let Some(parent) = root.parent() else {
        return Vec::new();
    };

    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };

    let mut found: Vec<Sibling> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let origin = origin_of(&path)?;
            (organisation_of(&origin).as_deref() == Some(organisation)).then_some(Sibling {
                root: path,
                origin_url: Some(origin),
            })
        })
        .collect();

    found.sort_by(|left, right| left.root.cmp(&right.root));
    found
}

/// Reads a checkout's origin without spawning git.
///
/// A directory of fifty checkouts would otherwise be fifty subprocesses, on a
/// path that runs while someone is waiting. Falls back to git for the layouts
/// this cannot parse — a linked worktree keeps a `.git` file rather than a
/// directory, and its config lives elsewhere.
fn origin_of(path: &Path) -> Option<String> {
    let git_path = path.join(".git");
    if !git_path.exists() {
        return None;
    }

    if git_path.is_dir()
        && let Ok(config) = std::fs::read_to_string(git_path.join("config"))
        && let Some(url) = origin_url_in(&config)
    {
        return Some(url);
    }

    git::discover(path).origin_url
}

/// Pulls `url` out of the `[remote "origin"]` section of a git config.
fn origin_url_in(config: &str) -> Option<String> {
    let mut in_origin = false;

    for line in config.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('[') {
            // Both spellings occur: `[remote "origin"]` from git itself, and
            // the subsection-on-one-line form some tools write.
            in_origin = trimmed.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }

        if in_origin
            && let Some((key, value)) = trimmed.split_once('=')
            && key.trim() == "url"
        {
            return Some(value.trim().to_owned());
        }
    }

    None
}
