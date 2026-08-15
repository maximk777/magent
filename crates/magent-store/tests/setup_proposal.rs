//! Noticing that a workspace is not set up, and proposing what to do.
//!
//! This is the case the design exists for. Fifty-odd checkouts of one
//! organisation sitting side by side is not fifty projects — it is one, and
//! what is learned about deployment or authentication in any of them is true of
//! all of them. Left ungrouped, that knowledge is filed under whichever
//! directory happened to be open, and never reaches the other forty-nine.
//!
//! So the proposal has to find them without being told, and it has to be
//! confident enough to be worth acting on and cautious enough not to sweep up
//! unrelated work sharing a parent directory.

use std::path::Path;

use magent_store::Store;

fn git(directory: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
}

fn repo(root: &Path, origin: Option<&str>) {
    std::fs::create_dir_all(root).expect("mkdir");
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.invalid"]);
    git(root, &["config", "user.name", "T"]);
    std::fs::write(root.join("README.md"), "seed\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "seed"]);
    if let Some(origin) = origin {
        git(root, &["remote", "add", "origin", origin]);
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("magent.db")).expect("open");
        Self { dir, store }
    }

    fn work(&self) -> std::path::PathBuf {
        self.dir.path().join("work")
    }
}

// --- finding the siblings ---------------------------------------------------

#[test]
fn checkouts_of_one_organisation_are_proposed_as_a_group() {
    let fixture = Fixture::new();
    for name in ["clients", "payments", "ledger"] {
        repo(
            &fixture.work().join(name),
            Some(&format!("git@github.com:wbbank/{name}.git")),
        );
    }

    let proposal = fixture
        .store
        .propose_grouping(&fixture.work().join("clients"))
        .expect("propose");

    // These sit under `work`, which is a place to keep code rather than a
    // name for it, so the organisation names the group instead.
    assert_eq!(proposal.suggested_name.as_deref(), Some("wbbank"));
    assert_eq!(
        proposal.organisation.as_deref(),
        Some("github.com/wbbank"),
        "the organisation stays on the proposal as the alternative to offer"
    );
    let mut found: Vec<_> = proposal
        .siblings
        .iter()
        .filter_map(|sibling| sibling.root.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(found, ["clients", "ledger", "payments"]);
}

/// The directory being worked in belongs in its own group. Leaving it out
/// would produce a group of everything except the thing that prompted it.
#[test]
fn the_current_repository_is_part_of_what_is_proposed() {
    let fixture = Fixture::new();
    for name in ["clients", "payments"] {
        repo(
            &fixture.work().join(name),
            Some(&format!("git@github.com:wbbank/{name}.git")),
        );
    }

    let proposal = fixture
        .store
        .propose_grouping(&fixture.work().join("clients"))
        .expect("propose");

    assert!(
        proposal
            .siblings
            .iter()
            .any(|sibling| sibling.root.ends_with("clients")),
        "the repository that prompted the proposal is missing from it"
    );
}

/// The failure worth designing against. A parent directory is where unrelated
/// work also lives, and a proposal that swept up a personal side project would
/// leak a bank's memory into it.
#[test]
fn unrelated_checkouts_beside_them_are_left_out() {
    let fixture = Fixture::new();
    for name in ["clients", "payments"] {
        repo(
            &fixture.work().join(name),
            Some(&format!("git@github.com:wbbank/{name}.git")),
        );
    }
    repo(
        &fixture.work().join("blog"),
        Some("git@github.com:someone/blog.git"),
    );
    repo(&fixture.work().join("scratch"), None);

    let proposal = fixture
        .store
        .propose_grouping(&fixture.work().join("clients"))
        .expect("propose");

    let names: Vec<_> = proposal
        .siblings
        .iter()
        .filter_map(|sibling| sibling.root.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    assert!(!names.contains(&"blog".to_owned()), "{names:?}");
    assert!(
        !names.contains(&"scratch".to_owned()),
        "a repository with no origin shares nothing to group on: {names:?}"
    );
}

/// The SSH and HTTPS forms of one organisation are one organisation. This is
/// the same normalisation identity uses, and a proposal that split on it would
/// offer two groups for one company.
#[test]
fn the_url_form_does_not_split_an_organisation() {
    let fixture = Fixture::new();
    repo(
        &fixture.work().join("clients"),
        Some("git@github.com:wbbank/clients.git"),
    );
    repo(
        &fixture.work().join("payments"),
        Some("https://github.com/wbbank/payments"),
    );

    let proposal = fixture
        .store
        .propose_grouping(&fixture.work().join("clients"))
        .expect("propose");

    assert_eq!(proposal.siblings.len(), 2, "{proposal:?}");
}

/// A host is not an organisation. Two projects on github.com have nothing in
/// common, and grouping on the host alone would put the whole world in one
/// workspace.
#[test]
fn a_shared_host_is_not_enough() {
    let fixture = Fixture::new();
    repo(
        &fixture.work().join("clients"),
        Some("git@github.com:wbbank/clients.git"),
    );
    repo(
        &fixture.work().join("thing"),
        Some("git@github.com:other/thing.git"),
    );

    let proposal = fixture
        .store
        .propose_grouping(&fixture.work().join("clients"))
        .expect("propose");

    assert_eq!(proposal.siblings.len(), 1, "{proposal:?}");
    assert_eq!(
        proposal.suggested_name, None,
        "one repository is not a group, and offering to make one wastes a decision"
    );
}

#[test]
fn a_directory_that_is_not_a_repository_proposes_nothing() {
    let fixture = Fixture::new();
    let plain = fixture.dir.path().join("notes");
    std::fs::create_dir_all(&plain).expect("mkdir");

    let proposal = fixture.store.propose_grouping(&plain).expect("propose");

    assert!(proposal.siblings.is_empty());
    assert_eq!(proposal.suggested_name, None);
}

// --- knowing when it has already been done ---------------------------------

#[test]
fn an_already_grouped_workspace_is_reported_as_settled() {
    let fixture = Fixture::new();
    let roots: Vec<_> = ["clients", "payments"]
        .iter()
        .map(|name| {
            let root = fixture.work().join(name);
            repo(&root, Some(&format!("git@github.com:wbbank/{name}.git")));
            root
        })
        .collect();

    let before = fixture.store.propose_grouping(&roots[0]).expect("propose");
    assert!(!before.already_grouped);

    fixture
        .store
        .group_into_workspace("wbbank", &roots)
        .expect("group");

    let after = fixture.store.propose_grouping(&roots[0]).expect("propose");
    assert!(
        after.already_grouped,
        "proposing a group that exists would ask the same question forever"
    );
    assert_eq!(after.workspace_name.as_deref(), Some("wbbank"));
}

/// Proposing is a read. Nothing about it may change the store, or an agent
/// that merely looked would have decided.
#[test]
fn proposing_changes_nothing() {
    let fixture = Fixture::new();
    let root = fixture.work().join("clients");
    repo(&root, Some("git@github.com:wbbank/clients.git"));

    let before = fixture.store.workspace_names().expect("names").len();
    fixture.store.propose_grouping(&root).expect("propose");
    fixture
        .store
        .propose_grouping(&root)
        .expect("propose again");

    assert_eq!(
        fixture.store.workspace_names().expect("names").len(),
        before,
        "a read created a workspace"
    );
}

// --- what to call it --------------------------------------------------------

/// Run against the real corpus, the organisation was `fintech` while the
/// person had already named the directory `wbbank` — and `wbbank` is what they
/// called the workspace when they grouped it by hand. A directory name is a
/// choice someone made; an organisation is an accident of where the code is
/// hosted.
#[test]
fn a_meaningful_parent_directory_names_the_group() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("wbbank");
    for name in ["clients", "payments"] {
        repo(
            &bank.join(name),
            Some(&format!("git@gitlab.example.com:fintech/{name}.git")),
        );
    }

    let proposal = fixture
        .store
        .propose_grouping(&bank.join("clients"))
        .expect("propose");

    assert_eq!(proposal.suggested_name.as_deref(), Some("wbbank"));
}

/// But not every parent is a name. Checkouts under `~/code` are not the "code"
/// project, and suggesting that would be worse than saying nothing useful.
#[test]
fn a_container_directory_does_not_name_the_group() {
    let fixture = Fixture::new();
    for parent in ["code", "src", "projects", "programming", "repos"] {
        let root = fixture.dir.path().join(parent);
        for name in ["clients", "payments"] {
            repo(
                &root.join(name),
                Some(&format!("git@gitlab.example.com:fintech/{name}.git")),
            );
        }

        let proposal = fixture
            .store
            .propose_grouping(&root.join("clients"))
            .expect("propose");

        assert_eq!(
            proposal.suggested_name.as_deref(),
            Some("fintech"),
            "{parent} is where code is kept, not what it is called"
        );
    }
}

/// Found by running this against a real machine: the checkouts sat directly in
/// the home directory, and the proposal offered to group them under the user's
/// own name. Repositories cloned into `$HOME` are not a project — that is where
/// everything lives — and a group made of them would put unrelated work into
/// one memory.
#[test]
fn checkouts_loose_in_the_home_directory_are_not_a_group() {
    let fixture = Fixture::new();
    let home = fixture.dir.path().join("home");
    for name in ["clients", "payments"] {
        repo(
            &home.join(name),
            Some(&format!("git@github.com:someone/{name}.git")),
        );
    }

    let proposal = fixture
        .store
        .propose_grouping_from(&home.join("clients"), Some(&home))
        .expect("propose");

    assert_eq!(
        proposal.suggested_name, None,
        "the home directory is where everything lives, not what it is called"
    );
    assert!(
        proposal.siblings.len() >= 2,
        "what was found is still reported; only the proposal is withheld"
    );
}

/// The same directory is a perfectly good group when it is not home.
#[test]
fn the_same_layout_elsewhere_is_still_proposed() {
    let fixture = Fixture::new();
    let bank = fixture.dir.path().join("wbbank");
    for name in ["clients", "payments"] {
        repo(
            &bank.join(name),
            Some(&format!("git@github.com:someone/{name}.git")),
        );
    }

    let proposal = fixture
        .store
        .propose_grouping_from(&bank.join("clients"), Some(fixture.dir.path()))
        .expect("propose");

    assert_eq!(proposal.suggested_name.as_deref(), Some("wbbank"));
}
