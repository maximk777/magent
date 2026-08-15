//! Magent's MCP surface.
//!
//! This is the portable half of the integration: instructions, tools and (in
//! later slices) prompts and resources travel to any MCP client, while hooks
//! stay Claude Code specific. The division is by guarantee — hooks fire whether
//! or not the model cooperates, so they own capture; MCP owns everything that
//! needs the model's own knowledge.

use std::{path::PathBuf, sync::Arc};

use magent_core::{
    CheckpointCommand, FinishRunCommand, HarnessKind, OperationId, RunId, StartRunCommand,
};
use magent_store::{Store, StoreError};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The bootstrap contract.
///
/// Claude Code loads this into the system prompt at session start and truncates
/// it at 2 KB, so the operative instructions come first and the caveats last.
/// It says when to call, not what Magent is: the model does not need the
/// architecture, only the protocol.
const INSTRUCTIONS: &str = "\
Magent gives this session durable task memory that survives context compaction, \
/clear, and handoff to another agent.

Call magent_start before non-trivial work. It opens or resumes a run and returns \
any earlier checkpoint. Pass resume_run_id when continuing a run named in \
restored context.

Call magent_checkpoint at stage boundaries, after a significant decision, and \
before handing work over. Record what only you know: decisions and the \
alternatives you rejected, what you verified and how. File edits are captured \
automatically, so do not restate them.

Call magent_finish with close_session when stepping away, or complete_run when \
the task is done and verified. Closing a session does not finish the task.

magent_status reports the current run and changes nothing.

Every mutating call takes an operation_id. Generate a fresh UUID per call, and \
reuse the same one when retrying, so a retry cannot duplicate state.

Magent does not replace native approvals: dangerous actions are still confirmed \
in this harness.";

/// What the client may supply when opening a run.
///
/// Deliberately smaller than [`StartRunCommand`]: the harness and the workspace
/// roots are resolved by the server from how it was launched. A client-supplied
/// value there would let a session misreport where and what it is.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct StartToolInput {
    /// Idempotency key. Reuse it when retrying the same call.
    pub operation_id: OperationId,
    /// What this run is for, in one line.
    pub task: String,
    /// Set to continue an existing run instead of opening a new one.
    #[serde(default)]
    pub resume_run_id: Option<RunId>,
    /// The harness's own session identifier, when it is known.
    #[serde(default)]
    pub external_session_hint: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    run: Option<magent_core::RunSnapshot>,
}

/// The MCP server. Holds the store and the identity the server was launched
/// with; it never learns either from the client.
#[derive(Clone)]
pub struct MagentMcp {
    store: Arc<Store>,
    harness: HarnessKind,
    workspace_roots: Vec<PathBuf>,
    tool_router: ToolRouter<Self>,
}

impl MagentMcp {
    /// Builds a server over `store`, working in `workspace_root`.
    #[must_use]
    pub fn new(store: Arc<Store>, harness: HarnessKind, workspace_root: PathBuf) -> Self {
        Self {
            store,
            harness,
            workspace_roots: vec![workspace_root],
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl MagentMcp {
    #[tool(description = "Report the current Magent run for this workspace. Read-only.")]
    async fn magent_status(&self) -> Result<String, String> {
        let run = self
            .store
            .latest_open_run_for_path(&self.workspace_roots[0])
            .map_err(|error| render_error(&error))?;

        render(&StatusReport { run })
    }

    #[tool(
        description = "Open a durable Magent run, or resume one from an earlier session or agent. Call before non-trivial work."
    )]
    async fn magent_start(
        &self,
        Parameters(input): Parameters<StartToolInput>,
    ) -> Result<String, String> {
        let command = StartRunCommand {
            operation_id: input.operation_id,
            task: input.task,
            resume_run_id: input.resume_run_id,
            external_session_hint: input.external_session_hint,
            workspace_roots: self.workspace_roots.clone(),
        };

        render(
            &self
                .store
                .adopt_or_start_run(&command, self.harness)
                .map_err(|error| render_error(&error))?,
        )
    }

    #[tool(
        description = "Persist what only you know about this run: decisions, alternatives rejected, what was verified, open risks. Call at stage boundaries and before handing work over."
    )]
    async fn magent_checkpoint(
        &self,
        Parameters(command): Parameters<CheckpointCommand>,
    ) -> Result<String, String> {
        render(
            &self
                .store
                .save_checkpoint(&command)
                .map_err(|error| render_error(&error))?,
        )
    }

    #[tool(
        description = "Close this session (close_session) or complete the whole run (complete_run). Closing a session does not finish the task."
    )]
    async fn magent_finish(
        &self,
        Parameters(command): Parameters<FinishRunCommand>,
    ) -> Result<String, String> {
        render(
            &self
                .store
                .finish_run(&command)
                .map_err(|error| render_error(&error))?,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MagentMcp {
    fn get_info(&self) -> ServerInfo {
        // Both types are #[non_exhaustive], so they are built from their
        // defaults and adjusted rather than constructed literally.
        let mut identity = Implementation::from_build_env();
        identity.name = "magent".into();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = identity;
        info.instructions = Some(INSTRUCTIONS.to_owned());
        info
    }
}

/// Serialises a successful result as compact JSON.
fn render<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| {
        serde_json::json!({
            "code": "mcp_result_serialization_failed",
            "message": error.to_string(),
        })
        .to_string()
    })
}

/// Renders a failure as a tool error carrying a stable code.
///
/// The model needs to tell "you sent something invalid" from "this run is
/// already finished" in order to react differently, so the code travels with
/// the message rather than being flattened into prose.
fn render_error(error: &StoreError) -> String {
    serde_json::json!({
        "code": error.code(),
        "message": error.to_string(),
    })
    .to_string()
}
