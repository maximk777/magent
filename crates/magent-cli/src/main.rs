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
