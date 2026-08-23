//! The headless Claude Code engine.
//!
//! Distillation runs on the user's subscription rather than an API key: there
//! is no second bill and no extra secret in the config. The cost is process
//! startup, which is irrelevant to a background worker.

use std::{
    io::Read,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::{DistillRequest, Distillation, Distiller};

/// Stops a child and everything it started.
///
/// Through `kill` rather than a syscall: killing a group needs `killpg`, which
/// would mean declaring `libc` here for one call. The command is on every unix
/// this runs on, and a failure to signal is deliberately ignored — the group is
/// already gone, which is the outcome wanted.
fn stop_process_group(child: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{child}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// How often the worker asks whether the child has finished.
///
/// Short enough that the bound is honoured to a fraction of a second, long
/// enough that waiting three minutes costs a few thousand cheap syscalls rather
/// than a spinning core.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Transcript bytes handed to the model.
///
/// Long sessions produce very large transcripts; the tail is where the current
/// state lives, so that is what gets read.
const TRANSCRIPT_TAIL_BYTES: usize = 96 * 1024;

/// Set on the nested `claude` process so our hooks stand down inside it.
///
/// The distiller runs the same CLI this integration hooks into. Left to
/// itself that session would open a run, compact, and queue another
/// distillation — a loop that bills itself. The hook checks this and returns
/// before it touches the store.
pub const RECURSION_GUARD: &str = "MAGENT_DISTILLING";

/// What `claude --output-format json` wraps the reply in.
#[derive(Debug, Deserialize)]
struct HeadlessEnvelope {
    #[serde(default)]
    result: String,
    #[serde(default)]
    is_error: bool,
}

/// Runs `claude` in headless mode to distil a transcript.
pub struct ClaudeHeadless {
    binary: PathBuf,
    model: String,
    /// How long the child may run before it is stopped. Nothing else in this
    /// process waits without a bound, and this was the exception.
    timeout: Duration,
}

impl Default for ClaudeHeadless {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            timeout: crate::DISTILLATION_TIMEOUT,
            // The task is summarising text that is already written. A larger
            // model would cost more for the same answer.
            model: "haiku".to_owned(),
        }
    }
}

impl ClaudeHeadless {
    #[must_use]
    pub fn new(binary: PathBuf, model: String, timeout: Duration) -> Self {
        Self {
            binary,
            model,
            timeout,
        }
    }

    /// The arguments this engine invokes `claude` with.
    ///
    /// Exposed so a test can read them without running the model.
    ///
    /// `--bare` used to be here, to stop the nested session loading our hooks
    /// and queueing another distillation. It also skips keychain reads, and
    /// its own help says auth is then strictly `ANTHROPIC_API_KEY` — so on a
    /// subscription it could not authenticate at all, and every distillation
    /// this profile ever queued failed. The recursion guard moved to
    /// [`RECURSION_GUARD`], which the hook honours; a flag on someone else's
    /// CLI could never have been the right place for a guarantee we can make
    /// ourselves.
    #[must_use]
    pub fn command_arguments(&self) -> Vec<String> {
        vec![
            "-p".to_owned(),
            "--model".to_owned(),
            self.model.clone(),
            "--output-format".to_owned(),
            "json".to_owned(),
        ]
    }
}

impl Distiller for ClaudeHeadless {
    fn distill(&self, request: &DistillRequest) -> anyhow::Result<Distillation> {
        let transcript = read_tail(&request.transcript)?;
        let prompt = build_prompt(&request.task, &transcript);

        let mut child = Command::new(&self.binary)
            .args(self.command_arguments())
            .arg(&prompt)
            .env(RECURSION_GUARD, "1")
            // Without this the CLI waits several seconds for input that will
            // never arrive.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so stopping it stops what it started.
            // `claude` runs children of its own, and killing only the process
            // we spawned leaves those holding the write end of the pipe: the
            // reads below would then never return, and the bound would hang on
            // the very child it just stopped.
            .process_group(0)
            .spawn()?;

        // Drained on threads for as long as the child runs, which is not
        // tidiness. A pipe holds about sixty-four kilobytes; a child that
        // writes more blocks in `write` until somebody reads, so a worker
        // polling for exit while nobody drains would wait for ever on exactly
        // the child this deadline exists to catch. `Command::output` did this
        // correctly, and replacing it inherits the obligation rather than
        // dropping it.
        let mut child_out = child.stdout.take().expect("stdout was piped");
        let mut child_err = child.stderr.take().expect("stderr was piped");
        let out_reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = child_out.read_to_end(&mut buffer);
            buffer
        });
        let err_reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = child_err.read_to_end(&mut buffer);
            buffer
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                stop_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(POLL_INTERVAL);
        };

        // After the kill rather than before: killing the child closes its ends
        // of the pipes, which is what lets both reads return.
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();

        let Some(status) = status else {
            anyhow::bail!(
                "claude did not finish within {:?} and was stopped; the reasoning was not going to arrive",
                self.timeout
            );
        };

        let envelope: Option<HeadlessEnvelope> = serde_json::from_slice(&stdout).ok();

        if !status.success() {
            // The reason is in the envelope on stdout, not on stderr: a
            // refused login reports "Not logged in" there and leaves stderr
            // empty. Reading only the exit status recorded a failure whose
            // stored message ended at the colon, and it stayed unexplained
            // for as long as anyone cared to look.
            let reason = envelope
                .as_ref()
                .map(|envelope| envelope.result.trim())
                .filter(|reason| !reason.is_empty())
                .map_or_else(
                    || String::from_utf8_lossy(&stderr).trim().to_owned(),
                    ToOwned::to_owned,
                );
            anyhow::bail!("claude exited with {status}: {reason}");
        }

        let envelope = envelope.ok_or_else(|| {
            anyhow::anyhow!("claude returned something that was not the json envelope")
        })?;
        if envelope.is_error {
            anyhow::bail!("claude reported an error: {}", envelope.result.trim());
        }

        parse_distillation(&envelope.result)
    }
}

/// Extracts the JSON object from a reply that may be wrapped in prose or a
/// fenced block. Being strict here would throw away a usable answer.
fn parse_distillation(reply: &str) -> anyhow::Result<Distillation> {
    let start = reply
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("the reply contained no JSON object"))?;
    let end = reply
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("the reply contained no JSON object"))?;

    if end <= start {
        anyhow::bail!("the reply contained no JSON object");
    }

    Ok(serde_json::from_str(&reply[start..=end])?)
}

/// Reads the last [`TRANSCRIPT_TAIL_BYTES`] of a transcript.
fn read_tail(path: &std::path::Path) -> anyhow::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(TRANSCRIPT_TAIL_BYTES as u64);
    file.seek(SeekFrom::Start(offset))?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    // A mid-character cut is possible at the seek point, so the read is lossy
    // rather than fallible.
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn build_prompt(task: &str, transcript: &str) -> String {
    format!(
        "You are summarising a coding session so another agent can continue it \
without reading the transcript.\n\n\
Task: {task}\n\n\
Return ONLY a JSON object with these keys, no prose and no code fence:\n\
{{\"completed_steps\":[],\"next_steps\":[],\"decisions\":[],\"rejected\":[],\
\"verification\":[],\"risks\":[],\"handoff_summary\":\"\"}}\n\n\
Rules:\n\
- decisions: choices made and why, not what was typed.\n\
- rejected: alternatives considered and turned down, so they are not \
re-proposed.\n\
- verification: what was actually checked, and its result. Do not claim a \
check that was not run.\n\
- risks: what is unresolved or might be wrong.\n\
- handoff_summary: two sentences at most, stating where the work stands.\n\
- Omit file edits: those are recorded separately.\n\
- Every entry must be supported by the transcript. Leave a list empty rather \
than guessing.\n\n\
Transcript (may be truncated at the start):\n{transcript}"
    )
}
