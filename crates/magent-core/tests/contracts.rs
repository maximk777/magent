//! Contract tests for the Magent domain layer.
//!
//! These pin the wire shape and validation rules that the store, the MCP server
//! and the hook binary all depend on. They must not touch the filesystem.

use std::path::PathBuf;

use magent_core::{
    CheckpointCommand, CheckpointOrigin, FinishAction, FinishRunCommand, OperationId, RunId,
    SessionId, StartRunCommand, TaskDone, Validate, WorkflowStage,
};

fn workspace_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/tmp/magent-fixture")]
}

fn valid_start() -> StartRunCommand {
    StartRunCommand {
        operation_id: OperationId::new(),
        task: "fix payment timeout".into(),
        resume_run_id: None,
        external_session_hint: None,
        workspace_roots: workspace_roots(),
    }
}

fn valid_checkpoint(run_id: RunId, session_id: SessionId) -> CheckpointCommand {
    CheckpointCommand {
        operation_id: OperationId::new(),
        run_id,
        session_id,
        stage: WorkflowStage::Executing,
        origin: CheckpointOrigin::Deterministic,
        completed_steps: vec!["located owner".into()],
        next_steps: vec!["write regression test".into()],
        decisions: vec!["keep public API compatible".into()],
        rejected: vec!["rewriting the client was out of scope".into()],
        changed_files: vec!["src/service.rs".into()],
        verification: vec!["targeted test is red".into()],
        risks: vec![],
        handoff_summary: "Owner traced; regression test is next.".into(),
        task_done: None,
        binding: None,
    }
}

fn valid_tick() -> TaskDone {
    TaskDone {
        number: "1.2".into(),
        verify_command: "cargo test -p worker retry_budget".into(),
        output: "test retry_budget ... ok".into(),
    }
}

// --- validation ------------------------------------------------------------

#[test]
fn start_rejects_blank_task() {
    let command = StartRunCommand {
        task: "   ".into(),
        ..valid_start()
    };

    assert_eq!(command.validate().unwrap_err().code(), "invalid_task");
}

#[test]
fn start_rejects_missing_workspace_root() {
    let command = StartRunCommand {
        workspace_roots: vec![],
        ..valid_start()
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "missing_workspace_root"
    );
}

#[test]
fn start_accepts_a_well_formed_command() {
    assert!(valid_start().validate().is_ok());
}

#[test]
fn checkpoint_rejects_blank_handoff_summary() {
    let command = CheckpointCommand {
        handoff_summary: "\n\t ".into(),
        ..valid_checkpoint(RunId::new(), SessionId::new())
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_handoff_summary"
    );
}

/// `completed` is reachable only through `magent_finish`. A checkpoint claiming
/// it would let a run look finished while its session is still open.
#[test]
fn checkpoint_rejects_completed_stage() {
    let command = CheckpointCommand {
        stage: WorkflowStage::Completed,
        ..valid_checkpoint(RunId::new(), SessionId::new())
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_checkpoint_stage"
    );
}

#[test]
fn checkpoint_accepts_a_well_formed_command() {
    assert!(
        valid_checkpoint(RunId::new(), SessionId::new())
            .validate()
            .is_ok()
    );
}

/// A tick with no output is the whole failure mode this evidence exists to
/// prevent: a task marked done on the word of the agent that did it, with
/// nothing anyone can read to see the verification ran.
#[test]
fn a_tick_without_output_is_refused() {
    let command = CheckpointCommand {
        task_done: Some(TaskDone {
            output: "  ".into(),
            ..valid_tick()
        }),
        ..valid_checkpoint(RunId::new(), SessionId::new())
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_task_output"
    );
}

/// Its own variant rather than the output's: the caller sees only the code, so
/// one that covered both fields would leave it guessing which it left blank.
#[test]
fn a_tick_without_a_command_is_refused() {
    let command = CheckpointCommand {
        task_done: Some(TaskDone {
            verify_command: "\t".into(),
            ..valid_tick()
        }),
        ..valid_checkpoint(RunId::new(), SessionId::new())
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_verify_command"
    );
}

/// The number addresses a task the plan already wrote down, so it is held to
/// the same shape `magent_plan` accepted it in — otherwise a tick can name a
/// task that could never have existed.
#[test]
fn a_tick_numbered_like_a_path_is_refused() {
    let command = CheckpointCommand {
        task_done: Some(TaskDone {
            number: "1..2".into(),
            ..valid_tick()
        }),
        ..valid_checkpoint(RunId::new(), SessionId::new())
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_task_number"
    );
}

#[test]
fn finish_rejects_blank_outcome() {
    let command = FinishRunCommand {
        operation_id: OperationId::new(),
        run_id: RunId::new(),
        session_id: SessionId::new(),
        action: FinishAction::CompleteRun,
        outcome: "  ".into(),
    };

    assert_eq!(command.validate().unwrap_err().code(), "invalid_outcome");
}

// --- wire shape ------------------------------------------------------------

#[test]
fn checkpoint_round_trips_as_json() {
    let command = valid_checkpoint(RunId::new(), SessionId::new());

    let encoded = serde_json::to_string(&command).expect("serialize");
    let decoded: CheckpointCommand = serde_json::from_str(&encoded).expect("deserialize");

    assert_eq!(decoded, command);
}

/// Ids are transparent strings on the wire, not `{"0": "..."}` wrappers. The
/// hook binary and the Web UI both read these by hand.
#[test]
fn ids_serialize_as_bare_strings() {
    let run_id = RunId::new();
    let encoded = serde_json::to_string(&run_id).expect("serialize");

    assert!(
        encoded.starts_with('"'),
        "expected a JSON string: {encoded}"
    );
    assert_eq!(
        encoded.trim_matches('"').len(),
        36,
        "expected a hyphenated uuid: {encoded}"
    );
}

#[test]
fn stage_and_origin_serialize_as_snake_case() {
    assert_eq!(
        serde_json::to_string(&WorkflowStage::Executing).unwrap(),
        "\"executing\""
    );
    assert_eq!(
        serde_json::to_string(&CheckpointOrigin::Deterministic).unwrap(),
        "\"deterministic\""
    );
}

/// The client never names the harness: the server knows which harness it was
/// launched for. A client-supplied field would let a session lie about itself.
#[test]
fn start_command_has_no_client_supplied_harness_field() {
    let encoded = serde_json::to_value(valid_start()).expect("serialize");
    let object = encoded.as_object().expect("object");

    assert!(
        !object.contains_key("harness"),
        "start command must not expose a harness field: {object:?}"
    );
}

// --- finish semantics ------------------------------------------------------

/// Closing a session hands work over; completing the run ends it. Conflating
/// them would make every harness switch look like a finished task.
#[test]
fn close_session_is_distinct_from_complete_run() {
    assert_ne!(FinishAction::CloseSession, FinishAction::CompleteRun);

    assert_eq!(
        serde_json::to_string(&FinishAction::CloseSession).unwrap(),
        "\"close_session\""
    );
    assert_eq!(
        serde_json::to_string(&FinishAction::CompleteRun).unwrap(),
        "\"complete_run\""
    );
}
