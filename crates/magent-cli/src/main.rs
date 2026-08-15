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
enum Command {
    /// Handle a harness lifecycle event. Reads the event JSON on stdin.
    Hook {
        /// Event name, for example `session-start` or `pre-compact`.
        event: String,
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
        Command::Import {
            memory_dir,
            codex_rollouts,
            state_dir,
        } => run_import(memory_dir, codex_rollouts, state_dir),
        Command::Mcp { state_dir } => run_mcp(state_dir),
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
        let server = magent_mcp::MagentMcp::new(store, HarnessKind::ClaudeCode, workspace_root);
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
fn report(message: &str) {
    let _ = writeln!(std::io::stderr(), "magent: {message}");
}
