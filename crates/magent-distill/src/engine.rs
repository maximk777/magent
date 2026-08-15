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
    /// Exposed so a test can assert `--bare` is present without running the
    /// model. That flag is load-bearing: it skips hooks, plugins, MCP servers,
    /// auto memory and `CLAUDE.md`, which is what stops the distiller from
    /// starting a session that opens a run, compacts, and queues another
    /// distillation — a loop that bills itself.
    #[must_use]
    pub fn command_arguments(&self) -> Vec<String> {
        vec![
            "--bare".to_owned(),
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
            // Without this the CLI waits several seconds for input that will
            // never arrive.
            .stdin(Stdio::null())
            .output()?;

        if !output.status.success() {
            anyhow::bail!(
                "claude exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let envelope: HeadlessEnvelope = serde_json::from_slice(&output.stdout)?;
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
