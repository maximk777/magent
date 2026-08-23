//! Turning a transcript into the reasoning a checkpoint cannot observe.
//!
//! The `PreCompact` hook records what is provably true — files touched, git
//! state, stage — and queues the rest. This crate drains that queue in the
//! background, so nothing the user waits on ever blocks on a model call.
//!
//! The engine sits behind [`Distiller`] for two reasons: the queue semantics
//! must be testable without spending money, and the choice of engine is a
//! policy decision that should not reach into the worker.

mod engine;

use std::{path::PathBuf, time::Duration};

use magent_core::{
    CheckpointCommand, CheckpointOrigin, OperationId, RunId, SessionId, WorkflowStage,
};
use magent_store::Store;
use serde::{Deserialize, Serialize};

pub use engine::{ClaudeHeadless, RECURSION_GUARD};

/// Job kind this worker drains.
pub const ENRICH_JOB: &str = "enrich_checkpoint";

/// What the worker asks the engine to read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DistillRequest {
    pub run_id: RunId,
    pub session_id: SessionId,
    /// What the run is for, so the engine can judge what is relevant.
    pub task: String,
    pub transcript: PathBuf,
}

/// The reasoning behind a stretch of work.
///
/// Everything here is something only a reader of the transcript knows; the
/// observable facts are already on the deterministic checkpoint.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Distillation {
    pub completed_steps: Vec<String>,
    pub next_steps: Vec<String>,
    pub decisions: Vec<String>,
    /// Alternatives considered and turned down. Without these a resumed session
    /// re-proposes what was already settled.
    pub rejected: Vec<String>,
    pub verification: Vec<String>,
    pub risks: Vec<String>,
    pub handoff_summary: String,
}

/// Reads a transcript and returns what it means.
pub trait Distiller {
    /// # Errors
    ///
    /// Returns any failure to produce a distillation; the worker turns it into
    /// a retry or a give-up.
    fn distill(&self, request: &DistillRequest) -> anyhow::Result<Distillation>;
}

/// How long one distillation may run before its child is stopped.
///
/// Shared by [`WorkerConfig`] and the engine's own default so the two cannot be
/// set to different numbers by accident. Three minutes: the work is one model
/// call over a transcript tail, and a call that has not answered in three
/// minutes was not going to.
pub const DISTILLATION_TIMEOUT: Duration = Duration::from_mins(3);

#[derive(Clone, Copy, Debug)]
pub struct WorkerConfig {
    /// How long one distillation may run before its child is killed.
    pub distillation_timeout: Duration,
    /// How long a failed job waits before it is offered again.
    pub retry_backoff: Duration,
}

impl WorkerConfig {
    /// How long a claimed job stays invisible to other workers.
    ///
    /// Derived rather than stated. A lease shorter than the bound would hand
    /// the same job to a second worker while the first is still running and
    /// still going to write its result — the failure the lease exists to
    /// prevent, turned inside out. Arithmetic cannot drift; the sentence that
    /// stood here instead already had, because nothing bounded a distillation
    /// at all.
    #[must_use]
    pub fn lease(&self) -> Duration {
        self.distillation_timeout * 2
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            distillation_timeout: DISTILLATION_TIMEOUT,
            retry_backoff: Duration::from_mins(1),
        }
    }
}

/// What one pass of the worker did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Nothing was queued.
    Idle,
    /// A run gained an enriched checkpoint.
    Enriched(RunId),
    /// The attempt failed; the job was requeued or given up on.
    Failed(String),
}

#[derive(Debug, Deserialize)]
struct JobPayload {
    run_id: RunId,
    session_id: SessionId,
    transcript_path: Option<PathBuf>,
}

/// Claims at most one queued job and processes it.
///
/// Deliberately one job per call: the worker is spawned detached by a hook, and
/// a long-running loop would outlive the session that started it.
///
/// # Errors
///
/// Returns an error only when the store itself is unusable. A failing engine is
/// reported as [`Outcome::Failed`], since that is expected operation rather
/// than a broken installation.
pub fn run_once(
    store: &Store,
    distiller: &dyn Distiller,
    config: &WorkerConfig,
) -> anyhow::Result<Outcome> {
    let Some(job) = store.claim_job(ENRICH_JOB, config.lease())? else {
        return Ok(Outcome::Idle);
    };

    match process(store, distiller, &job.payload_json) {
        Ok(run_id) => {
            store.complete_job(ENRICH_JOB, &job.job_key)?;
            Ok(Outcome::Enriched(run_id))
        }
        Err(error) => {
            let message = format!("{error:#}");
            store.fail_job(ENRICH_JOB, &job.job_key, &message, config.retry_backoff)?;
            Ok(Outcome::Failed(message))
        }
    }
}

fn process(store: &Store, distiller: &dyn Distiller, payload_json: &str) -> anyhow::Result<RunId> {
    let payload: JobPayload = serde_json::from_str(payload_json)?;

    let transcript = payload
        .transcript_path
        .ok_or_else(|| anyhow::anyhow!("the job carries no transcript path"))?;

    // Checked before the engine runs: distilling a transcript that is gone
    // cannot succeed, and finding that out from the model would be billed.
    if !transcript.is_file() {
        anyhow::bail!("transcript {} no longer exists", transcript.display());
    }

    let run = store.get_run(payload.run_id)?;
    let distillation = distiller.distill(&DistillRequest {
        run_id: payload.run_id,
        session_id: payload.session_id,
        task: run.task.clone(),
        transcript,
    })?;

    // The run may have been completed while the job waited; `completed` is not
    // a stage a checkpoint may claim.
    let stage = if run.stage == WorkflowStage::Completed {
        WorkflowStage::Reviewing
    } else {
        run.stage
    };

    let changed_files = run
        .latest_checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.changed_files.clone())
        .unwrap_or_default();

    let summary = if distillation.handoff_summary.trim().is_empty() {
        // A blank summary would be rejected by validation, and an enriched
        // checkpoint that says nothing is worse than none at all.
        anyhow::bail!("the distillation produced no handoff summary");
    } else {
        distillation.handoff_summary.clone()
    };

    store.save_checkpoint(&CheckpointCommand {
        operation_id: OperationId::new(),
        run_id: payload.run_id,
        session_id: payload.session_id,
        stage,
        origin: CheckpointOrigin::Enriched,
        completed_steps: distillation.completed_steps,
        next_steps: distillation.next_steps,
        decisions: distillation.decisions,
        rejected: distillation.rejected,
        changed_files,
        verification: distillation.verification,
        risks: distillation.risks,
        handoff_summary: summary,
        task_done: None,
        binding: None,
    })?;

    Ok(payload.run_id)
}
