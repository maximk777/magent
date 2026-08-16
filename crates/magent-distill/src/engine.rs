//! The headless Claude Code engine.
//!
//! Distillation runs on the user's subscription rather than an API key: there
//! is no second bill and no extra secret in the config. The cost is process
//! startup, which is irrelevant to a background worker.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

use serde::Deserialize;

use crate::{DistillRequest, Distillation, Distiller};

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
}

impl Default for ClaudeHeadless {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            // The task is summarising text that is already written. A larger
            // model would cost more for the same answer.
            model: "haiku".to_owned(),
        }
    }
}

impl ClaudeHeadless {
    #[must_use]
    pub fn new(binary: PathBuf, model: String) -> Self {
        Self { binary, model }
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

        let output = Command::new(&self.binary)
            .args(self.command_arguments())
            .arg(&prompt)
            .env(RECURSION_GUARD, "1")
            // Without this the CLI waits several seconds for input that will
            // never arrive.
            .stdin(Stdio::null())
            .output()?;

        let envelope: Option<HeadlessEnvelope> = serde_json::from_slice(&output.stdout).ok();

        if !output.status.success() {
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
                    || String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                    ToOwned::to_owned,
                );
            anyhow::bail!("claude exited with {}: {reason}", output.status);
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
