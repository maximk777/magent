//! What is wrong, and what to do about it.
//!
//! The question behind every run of this is the same: something is not working,
//! or might not be, and it is not obvious whether the cause is Magent, the
//! toolchain, or the change just made. So the report leads with what is missing
//! and stays quiet about what is fine — a diagnostic mostly made of ticks is one
//! people stop reading.
//!
//! It must also survive the thing it diagnoses. A doctor that needs a healthy
//! store to report an unhealthy one is useless exactly when it is wanted.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use magent_store::Store;

/// A language server, and how to get it.
///
/// Mirrors `lang-plugin/.lsp.json`; `scripts/check-plugin.sh` fails if the two
/// disagree, because a doctor that recommends a server the plugin does not
/// launch is worse than one that says nothing.
struct Server {
    /// The `toolchain-*` fact this follows from.
    toolchain: &'static str,
    command: &'static str,
    install: &'static str,
}

/// Only languages whose server has a one-line install. Naming a server without
/// a workable way to get it turns a finding into homework — which is why the
/// JVM is absent here despite being detected.
const SERVERS: [Server; 4] = [
    Server {
        toolchain: "toolchain-go",
        command: "gopls",
        install: "go install golang.org/x/tools/gopls@latest",
    },
    Server {
        toolchain: "toolchain-rust",
        command: "rust-analyzer",
        install: "rustup component add rust-analyzer",
    },
    Server {
        toolchain: "toolchain-node",
        command: "typescript-language-server",
        install: "npm install -g typescript-language-server typescript",
    },
    Server {
        toolchain: "toolchain-python",
        command: "pyright-langserver",
        install: "npm install -g pyright",
    },
];

/// Writes the report, returning false when something is actually broken.
///
/// A missing language server is a finding rather than a failure: plenty of
/// repositories are worked on without one, and exiting non-zero for it would
/// make the command useless in a script.
///
/// # Errors
/// Never. Failures are part of the report.
pub fn report(state_dir: &Path, here: &Path, out: &mut String) -> bool {
    let mut healthy = true;

    let _ = writeln!(out, "profile");
    let _ = writeln!(out, "  state     {}", state_dir.display());

    let database = crate::paths::database_path(state_dir);
    let store = match Store::open(&database) {
        Ok(store) => {
            let version = store
                .schema_version()
                .map_or_else(|_| "unknown".to_owned(), |version| version.to_string());
            let _ = writeln!(out, "  database  {} (schema {version})", database.display());
            Some(store)
        }
        Err(error) => {
            healthy = false;
            let _ = writeln!(out, "  database  {} — CANNOT OPEN", database.display());
            let _ = writeln!(out, "            {error}");
            let _ = writeln!(
                out,
                "            move it aside and Magent will make a new one; the old\n\
                 \x20           file is still readable with sqlite3"
            );
            None
        }
    };

    // Which binary this is. Two copies at different versions is a real state to
    // be in — one installed, one just built — and it explains behaviour that
    // otherwise looks impossible.
    if let Ok(binary) = std::env::current_exe() {
        let _ = writeln!(out, "  binary    {}", binary.display());
    }

    if let Some(store) = &store {
        report_workspace(store, here, out);
        report_queue(store, out);
    }

    report_toolchain(here, out);
    report_workflow(here, out);
    healthy
}

/// The `openspec` CLI, and whether this repository uses it.
///
/// The SDD skills this plugin ships call it for every artifact they produce.
/// Shipping skills that depend on a binary and never mentioning it leaves the
/// person to discover the dependency from a command that failed halfway
/// through a proposal.
///
/// Two separate states, because they fail differently: not having the tool is
/// "install something", and having it in a repository that has never run
/// `openspec init` is "start one here".
fn report_workflow(here: &Path, out: &mut String) {
    let installed = on_path("openspec");
    let root = magent_store::repository_root(here).unwrap_or_else(|| here.to_path_buf());
    let initialised = root.join("openspec").is_dir();

    if installed && initialised {
        return;
    }

    let _ = writeln!(out, "\nspec-driven work");

    if installed {
        let _ = writeln!(out, "  openspec                 installed");
    } else {
        let _ = writeln!(out, "  openspec                 MISSING");
        let _ = writeln!(out, "  {:<24}   npm install -g @fission-ai/openspec", "");
        let _ = writeln!(
            out,
            "  {:<24}   the sdd-brainstorm, sdd-plan and sdd-execute skills call it",
            ""
        );
    }

    if !initialised {
        let _ = writeln!(out, "  this repository          not set up for specs");
        // --tools is not optional: a bare `openspec init` exits with a list
        // of thirty editor names and creates nothing.
        let _ = writeln!(out, "  {:<24}   openspec init --tools claude", "");
    }
}

fn report_workspace(store: &Store, here: &Path, out: &mut String) {
    let _ = writeln!(out, "\nworkspace");

    match store.propose_grouping(here) {
        Ok(proposal) => {
            let _ = writeln!(out, "  here      {}", proposal.root.display());
            match (&proposal.workspace_name, &proposal.suggested_name) {
                (Some(name), _) => {
                    let _ = writeln!(out, "  grouped   {name}");
                }
                (None, Some(suggested)) => {
                    let _ = writeln!(
                        out,
                        "  grouped   no — {} checkouts here share {}",
                        proposal.siblings.len(),
                        proposal
                            .organisation
                            .as_deref()
                            .unwrap_or("an organisation"),
                    );
                    let _ = writeln!(
                        out,
                        "            memory learned in one will not reach the others until\n\
                         \x20           they are grouped: magent workspace group --name {suggested} ..."
                    );
                }
                (None, None) => {
                    let _ = writeln!(out, "  grouped   n/a — nothing here to group");
                }
            }
        }
        Err(error) => {
            let _ = writeln!(out, "  here      could not resolve: {error}");
        }
    }
}

fn report_queue(store: &Store, out: &mut String) {
    let Ok(counts) = store.job_counts() else {
        return;
    };
    let (pending, failed) = counts;

    // Silent when there is nothing to say. A queue that is empty is the normal
    // state and printing it every time trains people past the line that matters.
    if pending == 0 && failed == 0 {
        return;
    }

    let _ = writeln!(out, "\nbackground work");
    if pending > 0 {
        let _ = writeln!(out, "  pending   {pending}");
    }
    if failed > 0 {
        let _ = writeln!(out, "  failed    {failed}");
        let _ = writeln!(out, "            magent distill --once retries one");
    }
}

fn report_toolchain(here: &Path, out: &mut String) {
    let root = magent_store::repository_root(here).unwrap_or_else(|| here.to_path_buf());
    let detected = magent_store::detect_toolchain(&root);

    if detected.is_empty() {
        return;
    }

    let _ = writeln!(out, "\ntoolchain");
    for fact in &detected {
        // Config facts — a linter's settings file, a Makefile — are about the
        // same toolchain rather than another one, and listing them here would
        // repeat the language once per file found.
        let Some(server) = SERVERS.iter().find(|server| server.toolchain == fact.name) else {
            if fact.name.starts_with("toolchain-") {
                let _ = writeln!(out, "  {:<24} no language server offered", fact.title);
            }
            continue;
        };

        if on_path(server.command) {
            let _ = writeln!(out, "  {:<24} {} installed", fact.title, server.command);
        } else {
            let _ = writeln!(out, "  {:<24} {} MISSING", fact.title, server.command);
            let _ = writeln!(out, "  {:<24}   {}", "", server.install);
        }
    }
}

/// Whether `command` can be found the way a launcher would find it.
///
/// Deliberately not `which`: this has to agree with how Claude Code starts a
/// language server, which is by name against `PATH`.
fn on_path(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path).any(|directory| is_executable(&directory.join(command)))
}

#[cfg(unix)]
fn is_executable(path: &PathBuf) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &PathBuf) -> bool {
    path.is_file()
}
