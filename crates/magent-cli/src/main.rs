use std::io::{Read, Write};

use clap::{Parser, Subcommand};
use magent_cli::{hook, paths};

#[derive(Parser)]
#[command(
    name = "magent",
    version,
    about = "Durable task memory for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// Put repositories into one named workspace.
    Group {
        /// The workspace's name.
        #[arg(long)]
        name: String,

        /// Repository paths. Directories that are not repositories are grouped
        /// by path instead of being refused.
        paths: Vec<std::path::PathBuf>,
    },

    /// Move an imported namespace's facts up to a workspace, so they reach
    /// every repository in it.
    Promote {
        /// The namespace as imported, for example `wbbank-project-expert`.
        #[arg(long)]
        namespace: String,

        /// The workspace to promote into.
        #[arg(long)]
        into: String,
    },

    /// Show what workspaces exist and how many repositories each holds.
    List,
}

#[derive(Subcommand)]
enum DepsAction {
    /// Declare a repository to read, and check it out.
    Add {
        /// Any git URL, or a path. The SSH and HTTPS forms of one project are
        /// the same dependency.
        url: String,

        /// A branch, tag or commit. Defaults to the remote's default branch.
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },

    /// Show what is declared, where it is, and whether it is on disk.
    List,

    /// Bring every checkout up to date.
    Sync,

    /// Drop a declaration and delete its sources.
    Remove {
        /// The URL as given, or the project part of it.
        url: String,

        /// The ref, when the same project is declared at more than one.
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Handle a harness lifecycle event. Reads the event JSON on stdin.
    Hook {
        /// Event name, for example `session-start` or `pre-compact`.
        event: String,
    },

    /// Gather repositories that belong together, and move memory up to them.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,

        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },

    /// Import existing memory into the store.
    Import {
        /// A `~/memory`-style directory: one subdirectory per project.
        #[arg(long)]
        memory_dir: Option<std::path::PathBuf>,

        /// A Codex `rollout_summaries` directory.
        #[arg(long)]
        codex_rollouts: Option<std::path::PathBuf>,

        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },

    /// Open the local console for managing memory.
    Web {
        /// Port to listen on. Loopback only.
        #[arg(long, default_value_t = 7717)]
        port: u16,

        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },

    /// Write memory back out as a markdown corpus.
    Export {
        /// Directory to write into. Created if missing; existing files with the
        /// same names are overwritten.
        #[arg(long)]
        into: std::path::PathBuf,

        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },

    /// Drain the background distillation queue.
    Distill {
        /// Process one job and exit. This is how hooks invoke the worker.
        #[arg(long, default_value_t = true)]
        once: bool,

        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },

    /// Track repositories this workspace reads but does not work in.
    Deps {
        #[command(subcommand)]
        action: DepsAction,

        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },

    /// Report what is wrong, and what to do about it.
    Doctor {
        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },

    /// Serve the Magent MCP tools over stdio.
    Mcp {
        /// State directory. Defaults to `$MAGENT_STATE_DIR`, then `~/.magent`.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },
}

/// Hooks must never fail a session, so this process exits 0 no matter what.
///
/// A broken Magent costing the user their session would be far worse than a
/// Magent that quietly records nothing. Diagnostics go to stderr, which the
/// harness logs without injecting into the conversation.
fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Hook { event } => run_hook(&event),
        Command::Distill { once, state_dir } => run_distill(once, state_dir),
        Command::Export { into, state_dir } => run_export(&into, state_dir),
        Command::Web { port, state_dir } => run_web(port, state_dir),
        Command::Workspace { action, state_dir } => run_workspace(action, state_dir),
        Command::Import {
            memory_dir,
            codex_rollouts,
            state_dir,
        } => run_import(memory_dir, codex_rollouts, state_dir),
        Command::Deps { action, state_dir } => run_deps(action, state_dir),
        Command::Doctor { state_dir } => run_doctor(state_dir),
        Command::Mcp { state_dir } => run_mcp(state_dir),
    }
}

/// Serves the console.
///
/// Loopback only: it is an unauthenticated read-write view of a personal
/// memory, and reachable from elsewhere it would be a way in.
fn run_web(port: u16, state_dir: Option<std::path::PathBuf>) {
    use std::sync::Arc;

    use magent_store::Store;
    use magent_web::Console;

    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let database = paths::database_path(&state_dir);

    let store = match Store::open(&database) {
        Ok(store) => store,
        Err(error) => {
            report(&format!("could not open the store: {error}"));
            std::process::exit(1);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            report(&format!("could not start the runtime: {error}"));
            std::process::exit(1);
        }
    };

    let console = Console {
        store: Arc::new(store),
        database,
        deps_root: paths::deps_root(&state_dir),
    };

    if let Err(error) = runtime.block_on(magent_web::serve(console, port)) {
        report(&format!("console stopped: {error:#}"));
        std::process::exit(1);
    }
}

/// Writes memory back out as markdown.
fn run_export(into: &std::path::Path, state_dir: Option<std::path::PathBuf>) {
    use magent_store::Store;

    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let store = match Store::open(&paths::database_path(&state_dir)) {
        Ok(store) => store,
        Err(error) => {
            report(&format!("could not open the store: {error}"));
            std::process::exit(1);
        }
    };

    match magent_cli::export::export_memory_dir(&store, into) {
        Ok(summary) => println!(
            "exported {} fact(s) across {} namespace(s) into {}",
            summary.facts,
            summary.namespaces,
            summary.root.display()
        ),
        Err(error) => {
            report(&format!("export failed: {error:#}"));
            std::process::exit(1);
        }
    }
}

/// Groups repositories and promotes memory between scopes.
fn run_workspace(action: WorkspaceAction, state_dir: Option<std::path::PathBuf>) {
    use magent_store::Store;

    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let store = match Store::open(&paths::database_path(&state_dir)) {
        Ok(store) => store,
        Err(error) => {
            report(&format!("could not open the store: {error}"));
            std::process::exit(1);
        }
    };

    match action {
        WorkspaceAction::Group { name, paths } => match store.group_into_workspace(&name, &paths) {
            Ok(grouped) => {
                println!("workspace {name}: {} repositor(ies)", grouped.repositories);
                for (path, reason) in &grouped.skipped {
                    println!("  skipped {}: {reason}", path.display());
                }
            }
            Err(error) => {
                report(&format!("could not group: {error}"));
                std::process::exit(1);
            }
        },

        WorkspaceAction::Promote { namespace, into } => {
            let Ok(Some(workspace_id)) = store.workspace_id_by_name(&into) else {
                report(&format!(
                    "no workspace called {into}; group some repositories into it first"
                ));
                std::process::exit(1);
            };

            match store.promote_namespace(&namespace, workspace_id) {
                Ok(moved) => println!("promoted {moved} fact(s) from {namespace} to {into}"),
                Err(error) => {
                    report(&format!("could not promote: {error}"));
                    std::process::exit(1);
                }
            }
        }

        WorkspaceAction::List => match store.workspaces() {
            Ok(all) if all.is_empty() => println!("no workspaces yet"),
            Ok(all) => {
                for (name, repositories) in all {
                    println!("{name}\t{repositories} repositor(ies)");
                }
            }
            Err(error) => {
                report(&format!("could not list: {error}"));
                std::process::exit(1);
            }
        },
    }
}

/// Imports existing memory.
///
/// Reports what it could not read rather than failing the whole run: the corpus
/// is real and slightly ragged, and an importer that refuses the awkward tenth
/// is an importer nobody runs.
fn run_import(
    memory_dir: Option<std::path::PathBuf>,
    codex_rollouts: Option<std::path::PathBuf>,
    state_dir: Option<std::path::PathBuf>,
) {
    use magent_cli::import::{import_codex_rollouts, import_memory_dir};
    use magent_store::Store;

    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let store = match Store::open(&paths::database_path(&state_dir)) {
        Ok(store) => store,
        Err(error) => {
            report(&format!("could not open the store: {error}"));
            std::process::exit(1);
        }
    };

    let mut total = (0_usize, 0_usize, 0_usize);

    for (label, outcome) in [
        memory_dir.map(|dir| ("memory", import_memory_dir(&store, &dir))),
        codex_rollouts.map(|dir| ("codex", import_codex_rollouts(&store, &dir))),
    ]
    .into_iter()
    .flatten()
    {
        match outcome {
            Ok(summary) => {
                println!(
                    "{label}: {} fact(s), {} relation(s), {} skipped",
                    summary.facts,
                    summary.relations,
                    summary.skipped.len()
                );
                for (path, reason) in &summary.skipped {
                    println!("  skipped {}: {reason}", path.display());
                }
                total.0 += summary.facts;
                total.1 += summary.relations;
                total.2 += summary.skipped.len();
            }
            Err(error) => report(&format!("{label} import failed: {error:#}")),
        }
    }

    println!(
        "total: {} fact(s), {} relation(s), {} skipped",
        total.0, total.1, total.2
    );
}

/// Drains the distillation queue.
///
/// Spawned detached by a hook, so it must not assume a terminal, must not write
/// to the hook's stdout, and must exit rather than loop: a long-running worker
/// would outlive the session that started it.
fn run_distill(once: bool, state_dir: Option<std::path::PathBuf>) {
    use magent_distill::{ClaudeHeadless, Outcome, WorkerConfig, run_once};
    use magent_store::Store;

    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let store = match Store::open(&paths::database_path(&state_dir)) {
        Ok(store) => store,
        Err(error) => {
            report(&format!("could not open the store: {error}"));
            return;
        }
    };

    let engine = ClaudeHeadless::default();
    let config = WorkerConfig::default();

    loop {
        match run_once(&store, &engine, &config) {
            Ok(Outcome::Idle) => return,
            Ok(Outcome::Enriched(run_id)) => report(&format!("enriched run {run_id}")),
            Ok(Outcome::Failed(message)) => report(&format!("distillation failed: {message}")),
            Err(error) => {
                report(&format!("worker stopped: {error:#}"));
                return;
            }
        }

        if once {
            return;
        }
    }
}

/// Serves MCP on stdio.
///
/// Unlike a hook, this failing is worth reporting: the harness shows the server
/// as errored in `/mcp` instead of silently offering no tools. Nothing but
/// protocol frames may reach stdout, so every diagnostic goes to stderr.
fn run_mcp(state_dir: Option<std::path::PathBuf>) {
    use std::sync::Arc;

    use magent_core::HarnessKind;
    use magent_store::Store;
    use rmcp::{ServiceExt, transport::stdio};

    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            report(&format!("could not start the runtime: {error}"));
            std::process::exit(1);
        }
    };

    let result = runtime.block_on(async move {
        let store = Arc::new(Store::open(&paths::database_path(&state_dir))?);
        let server = magent_mcp::MagentMcp::new(
            store,
            HarnessKind::ClaudeCode,
            workspace_root,
            paths::deps_root(&state_dir),
        );
        let service = server.serve(stdio()).await?;
        service.waiting().await?;
        Ok::<(), anyhow::Error>(())
    });

    if let Err(error) = result {
        report(&format!("mcp server stopped: {error:#}"));
        std::process::exit(1);
    }
}

fn run_hook(event: &str) {
    let Some(event) = hook::Event::parse(event) else {
        report(&format!("unknown hook event: {event}"));
        return;
    };

    let mut raw = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut raw) {
        report(&format!("could not read hook input: {error}"));
        return;
    }

    let input = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            report(&format!("could not parse hook input: {error}"));
            return;
        }
    };

    match hook::handle(event, &input, &paths::state_dir()) {
        Ok(output) if output.is_empty() => {}
        Ok(output) => {
            let mut stdout = std::io::stdout();
            // A partial write would inject a truncated packet, which is worse
            // than injecting nothing at all.
            if let Err(error) = stdout.write_all(output.as_bytes()) {
                report(&format!("could not write hook output: {error}"));
            }
            let _ = stdout.flush();
        }
        Err(error) => report(&format!("hook failed: {error:#}")),
    }
}

/// Stderr only. Anything on stdout reaches the model.
/// Manages reference checkouts.
///
/// Every path this prints is meant to be pasted into a grep: the sources are
/// the deliverable, and a command that told you a dependency was "present"
/// without saying where would have done half the job.
fn run_deps(action: DepsAction, state_dir: Option<std::path::PathBuf>) {
    use magent_store::Store;

    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let deps_root = paths::deps_root(&state_dir);
    let store = match Store::open(&paths::database_path(&state_dir)) {
        Ok(store) => store,
        Err(error) => {
            report(&format!("could not open the store: {error}"));
            std::process::exit(1);
        }
    };

    // Dependencies belong to the workspace the terminal is standing in, the
    // same way a run does. Declaring one from inside a project is the whole
    // gesture.
    let here = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let workspace_id = match store.resolve_workspace_for(&here) {
        Ok(resolved) => resolved.workspace_id,
        Err(error) => {
            report(&format!("could not resolve this workspace: {error}"));
            std::process::exit(1);
        }
    };

    match action {
        DepsAction::Add { url, git_ref } => {
            deps_add(&store, workspace_id, &deps_root, &url, git_ref);
        }
        DepsAction::List => deps_list(&store, workspace_id, &deps_root),
        DepsAction::Sync => deps_sync(&store, workspace_id, &deps_root),
        DepsAction::Remove { url, git_ref } => {
            deps_remove(&store, workspace_id, &deps_root, &url, git_ref.as_deref());
        }
    }
}

fn deps_add(
    store: &magent_store::Store,
    workspace_id: magent_core::WorkspaceId,
    deps_root: &std::path::Path,
    url: &str,
    git_ref: Option<String>,
) {
    use magent_store::{DependencySpec, DependencyStatus, dependency_checkout};

    let declared = match store.declare_dependency(
        workspace_id,
        &DependencySpec {
            url: url.to_owned(),
            git_ref,
        },
    ) {
        Ok(declared) => declared,
        Err(error) => {
            report(&format!("could not declare {url}: {error}"));
            std::process::exit(1);
        }
    };

    match store.sync_dependency(declared.id, deps_root) {
        Ok(synced) if synced.status == DependencyStatus::Present => {
            println!("{}", synced.slug);
            println!(
                "  path: {}",
                dependency_checkout(deps_root, &synced).display()
            );
            if let Some(revision) = &synced.revision {
                println!("  at:   {}", short(revision));
            }
        }
        Ok(failed) => {
            // The declaration stays. The URL is usually right and the network
            // usually comes back, and re-typing it would be the wrong lesson.
            report(&format!(
                "declared {}, but could not fetch it: {}",
                failed.slug,
                failed.last_error.as_deref().unwrap_or("unknown reason")
            ));
            report("run `magent deps sync` to try again");
            std::process::exit(1);
        }
        Err(error) => {
            report(&format!("could not fetch: {error}"));
            std::process::exit(1);
        }
    }
}

fn deps_list(
    store: &magent_store::Store,
    workspace_id: magent_core::WorkspaceId,
    deps_root: &std::path::Path,
) {
    use magent_store::{DependencyStatus, dependency_checkout};

    let all = match store.dependencies(workspace_id) {
        Ok(all) => all,
        Err(error) => {
            report(&format!("could not list: {error}"));
            std::process::exit(1);
        }
    };

    if all.is_empty() {
        println!("no dependencies declared here");
        println!("add one with: magent deps add <git-url>");
        return;
    }

    for dependency in &all {
        let status = match dependency.status {
            DependencyStatus::Present => "present",
            DependencyStatus::Failed => "failed",
            DependencyStatus::Declared => "declared",
        };
        println!("{}\t{status}", dependency.slug);
        println!(
            "  path: {}",
            dependency_checkout(deps_root, dependency).display()
        );
        if let Some(revision) = &dependency.revision {
            println!("  at:   {}", short(revision));
        }
        if let Some(error) = &dependency.last_error {
            println!("  why:  {error}");
        }
    }
}

fn deps_sync(
    store: &magent_store::Store,
    workspace_id: magent_core::WorkspaceId,
    deps_root: &std::path::Path,
) {
    use magent_store::DependencyStatus;

    let all = match store.dependencies(workspace_id) {
        Ok(all) => all,
        Err(error) => {
            report(&format!("could not list: {error}"));
            std::process::exit(1);
        }
    };

    if all.is_empty() {
        println!("no dependencies declared here");
        return;
    }

    // One unreachable remote must not strand the rest: the common cause of a
    // failure here is a VPN that is not up, and the other nineteen checkouts
    // are still worth refreshing.
    let mut failures = 0;
    for dependency in &all {
        match store.sync_dependency(dependency.id, deps_root) {
            Ok(synced) if synced.status == DependencyStatus::Present => println!(
                "{}\t{}",
                synced.slug,
                synced.revision.as_deref().map_or_else(String::new, short)
            ),
            Ok(failed) => {
                failures += 1;
                report(&format!(
                    "{}: {}",
                    failed.slug,
                    failed.last_error.as_deref().unwrap_or("unknown reason")
                ));
            }
            Err(error) => {
                failures += 1;
                report(&format!("{}: {error}", dependency.slug));
            }
        }
    }

    if failures > 0 {
        report(&format!("{failures} of {} could not be fetched", all.len()));
    }
}

fn deps_remove(
    store: &magent_store::Store,
    workspace_id: magent_core::WorkspaceId,
    deps_root: &std::path::Path,
    url: &str,
    git_ref: Option<&str>,
) {
    let wanted = magent_store::normalize_origin(url);
    let all = store.dependencies(workspace_id).unwrap_or_default();
    let matches: Vec<_> = all
        .iter()
        .filter(|dependency| {
            dependency.identity_key == wanted
                && git_ref.is_none_or(|reference| dependency.git_ref.as_deref() == Some(reference))
        })
        .collect();

    match matches.as_slice() {
        [] => {
            report(&format!("nothing declared for {url}"));
            std::process::exit(1);
        }
        // Deleting sources is not the moment to guess which one was meant.
        [_, _, ..] if git_ref.is_none() => {
            report(&format!(
                "{url} is declared at several refs; name one with --ref:"
            ));
            for dependency in matches {
                report(&format!("  {}", dependency.slug));
            }
            std::process::exit(1);
        }
        found => {
            for dependency in found {
                if let Err(error) = store.forget_dependency(dependency.id, deps_root) {
                    report(&format!("could not remove {}: {error}", dependency.slug));
                    std::process::exit(1);
                }
                println!("removed {}", dependency.slug);
            }
        }
    }
}

/// Commits are cited by their first seven characters everywhere else.
fn short(revision: &str) -> String {
    revision.chars().take(7).collect()
}

/// Prints the diagnostic, exiting non-zero only when something is genuinely
/// broken. A missing language server is a finding, not a failure.
fn run_doctor(state_dir: Option<std::path::PathBuf>) {
    let state_dir = state_dir.unwrap_or_else(paths::state_dir);
    let here = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let mut out = String::new();
    let healthy = magent_cli::doctor::report(&state_dir, &here, &mut out);
    print!("{out}");

    if !healthy {
        std::process::exit(1);
    }
}

fn report(message: &str) {
    let _ = writeln!(std::io::stderr(), "magent: {message}");
}
