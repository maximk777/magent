//! Magent's MCP surface.
//!
//! This is the portable half of the integration: instructions, tools and (in
//! later slices) prompts and resources travel to any MCP client, while hooks
//! stay Claude Code specific. The division is by guarantee — hooks fire whether
//! or not the model cooperates, so they own capture; MCP owns everything that
//! needs the model's own knowledge.

use std::{path::PathBuf, sync::Arc};

use magent_core::{
    CheckpointCommand, CheckpointOrigin, Fact, FinishAction, FinishRunCommand, HarnessKind,
    OperationId, RememberCommand, RunId, SessionId, StartRunCommand, WorkflowStage,
};
use magent_store::{Dependency, FactContext, FactQuery, Store, StoreError, dependency_checkout};
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

Memory outlives the run. magent_search finds what is already known about this \
project and the user; magent_recall opens one fact by name, which is how the \
memory index at the top of a prompt is meant to be followed up. magent_remember \
records something durable: what is true of this codebase, or how the user wants \
to work, with the evidence for it. A fact marked verified must cite evidence.

magent_deps lists repositories checked out for reference and gives their paths \
on disk. Read and grep those files directly rather than guessing at a \
library's behaviour.

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

/// Free-text memory lookup.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct SearchToolInput {
    /// What to look for. A phrase works as well as keywords.
    pub text: String,
    /// How many facts to return. Small by default: these are read by a model
    /// with a context budget.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct RecallToolInput {
    /// The fact's name, as shown in the memory index.
    pub name: String,
}

/// What the client may supply when checkpointing.
///
/// Deliberately smaller than [`CheckpointCommand`]. Dogfooding this server
/// found the checkpoint — the one tool the whole design exists for — to be the
/// hardest to call: the domain command asks for a session id the server issued
/// and never told anyone, an origin only the server can judge, and all eight
/// list fields whether or not there is anything to put in them. A tool that
/// takes seven attempts to satisfy is a tool that gets skipped.
///
/// So the server answers what it can answer itself, and asks only for what only
/// the model knows.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct CheckpointToolInput {
    /// Idempotency key. Reuse it when retrying the same call.
    pub operation_id: OperationId,
    /// Where the work stands now.
    pub stage: WorkflowStage,
    /// What someone picking this up needs to know first, in a sentence or two.
    pub handoff_summary: String,
    /// The run to checkpoint. Omit to checkpoint the run open here.
    #[serde(default)]
    pub run_id: Option<RunId>,
    /// Omit: the server knows which session this is.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub completed_steps: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    /// Decisions taken, and why.
    #[serde(default)]
    pub decisions: Vec<String>,
    /// Alternatives considered and turned down. Without these a later session
    /// re-litigates settled questions.
    #[serde(default)]
    pub rejected: Vec<String>,
    /// Only files the harness would not have observed; edits are captured
    /// automatically.
    #[serde(default)]
    pub changed_files: Vec<String>,
    /// What was checked, and how.
    #[serde(default)]
    pub verification: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
}

/// What the client may supply when finishing.
///
/// Same reasoning as [`CheckpointToolInput`]: the run being finished is the run
/// in flight.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct FinishToolInput {
    /// Idempotency key. Reuse it when retrying the same call.
    pub operation_id: OperationId,
    /// `close_session` when stepping away, `complete_run` when the task is done.
    pub action: FinishAction,
    /// How it ended, in one line.
    pub outcome: String,
    /// Omit to finish the run open here.
    #[serde(default)]
    pub run_id: Option<RunId>,
    /// Omit: the server knows which session this is.
    #[serde(default)]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    facts: Vec<Fact>,
}

#[derive(Debug, Serialize)]
struct RecallResult {
    fact: Option<Fact>,
}

/// One reference checkout, as the agent needs to see it.
///
/// The path leads: everything else on this record is context for deciding
/// whether to trust what is at that path.
#[derive(Debug, Serialize)]
struct DependencyReport {
    path: PathBuf,
    slug: String,
    url: String,
    status: magent_store::DependencyStatus,
    git_ref: Option<String>,
    revision: Option<String>,
    /// Present only when the last attempt failed, so that stale sources are
    /// never presented as current ones.
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DepsReport {
    /// The directory holding every checkout. One grep here covers them all.
    root: PathBuf,
    dependencies: Vec<DependencyReport>,
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
    /// Where reference checkouts are materialised. Held rather than derived so
    /// the server reports the same paths the CLI wrote.
    deps_root: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl MagentMcp {
    /// Where memory written here is filed.
    ///
    /// Both a workspace id and a namespace: the id is the real binding, and the
    /// namespace is what groups new facts with the imported corpus for the same
    /// project.
    fn fact_context(&self) -> FactContext {
        let root = &self.workspace_roots[0];
        FactContext {
            workspace_id: self
                .store
                .resolve_workspace_for(root)
                .ok()
                .map(|resolved| resolved.workspace_id),
            run_id: None,
            namespace: root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            ..FactContext::default()
        }
    }

    fn query(&self, text: String, limit: Option<usize>) -> FactQuery {
        let root = magent_store::repository_root(&self.workspace_roots[0])
            .unwrap_or_else(|| self.workspace_roots[0].clone());

        FactQuery {
            text: Some(text),
            namespaces: magent_store::namespace_candidates(&root),
            // Without this, everything promoted to the workspace is invisible
            // to the tools: the visibility clause has nothing to match on.
            workspace_id: self
                .store
                .resolve_workspace_for(&root)
                .ok()
                .map(|resolved| resolved.workspace_id),
            limit: limit.unwrap_or(5).clamp(1, 25),
        }
    }

    fn report(&self, dependency: &Dependency) -> DependencyReport {
        DependencyReport {
            path: dependency_checkout(&self.deps_root, dependency),
            slug: dependency.slug.clone(),
            url: dependency.url.clone(),
            status: dependency.status,
            git_ref: dependency.git_ref.clone(),
            revision: dependency.revision.clone(),
            last_error: dependency.last_error.clone(),
        }
    }

    /// Fills in the run and session a tool was not told about.
    ///
    /// An explicit id always wins: guessing is a convenience for the common
    /// case, not a licence to override what the caller named.
    fn in_flight(
        &self,
        run_id: Option<RunId>,
        session_id: Option<SessionId>,
    ) -> Result<(RunId, SessionId), StoreError> {
        let run_id = match run_id {
            Some(named) => named,
            None => {
                self.store
                    .latest_open_run_for_path(&self.workspace_roots[0])?
                    .ok_or(StoreError::NoOpenRun)?
                    .run_id
            }
        };

        let session_id = match session_id {
            Some(named) => named,
            // Nothing open means the run was restored by a hook rather than
            // opened here. Recording against the run itself still beats
            // refusing: the checkpoint is what has to survive.
            None => self
                .store
                .latest_open_session(run_id)?
                .ok_or(StoreError::NoOpenRun)?,
        };

        Ok((run_id, session_id))
    }

    /// Builds a server over `store`, working in `workspace_root`, with
    /// reference checkouts under `deps_root`.
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        harness: HarnessKind,
        workspace_root: PathBuf,
        deps_root: PathBuf,
    ) -> Self {
        Self {
            store,
            harness,
            workspace_roots: vec![workspace_root],
            deps_root,
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
        description = "Persist what only you know about this run: decisions, alternatives rejected, what was verified, open risks. Call at stage boundaries and before handing work over. Only stage and handoff_summary are needed; the server knows which run and session this is."
    )]
    async fn magent_checkpoint(
        &self,
        Parameters(input): Parameters<CheckpointToolInput>,
    ) -> Result<String, String> {
        let (run_id, session_id) = self
            .in_flight(input.run_id, input.session_id)
            .map_err(|error| render_error(&error))?;

        let command = CheckpointCommand {
            operation_id: input.operation_id,
            run_id,
            session_id,
            stage: input.stage,
            // A checkpoint the model wrote is enriched by definition. Only the
            // hooks write deterministic ones, and they do not come through here.
            origin: CheckpointOrigin::Enriched,
            completed_steps: input.completed_steps,
            next_steps: input.next_steps,
            decisions: input.decisions,
            rejected: input.rejected,
            changed_files: input.changed_files,
            verification: input.verification,
            risks: input.risks,
            handoff_summary: input.handoff_summary,
        };

        render(
            &self
                .store
                .save_checkpoint(&command)
                .map_err(|error| render_error(&error))?,
        )
    }

    #[tool(
        description = "Search durable memory for what is already known about this project and the user. Free text; empty results are normal."
    )]
    async fn magent_search(
        &self,
        Parameters(input): Parameters<SearchToolInput>,
    ) -> Result<String, String> {
        let facts = self
            .store
            .search(&self.query(input.text, input.limit))
            .map_err(|error| render_error(&error))?;

        render(&SearchResult { facts })
    }

    #[tool(
        description = "Open one remembered fact by name. This is how to follow up a name shown in the memory index."
    )]
    async fn magent_recall(
        &self,
        Parameters(input): Parameters<RecallToolInput>,
    ) -> Result<String, String> {
        let fact = self
            .store
            .recall(&input.name, &self.fact_context())
            .map_err(|error| render_error(&error))?;

        render(&RecallResult { fact })
    }

    #[tool(
        description = "Record something durable: what is true of this codebase, or how the user wants to work, with the evidence for it."
    )]
    async fn magent_remember(
        &self,
        Parameters(command): Parameters<RememberCommand>,
    ) -> Result<String, String> {
        let fact_id = self
            .store
            .remember(&command, &self.fact_context())
            .map_err(|error| render_error(&error))?;

        render(&serde_json::json!({ "fact_id": fact_id }))
    }

    #[tool(
        description = "List the reference repositories checked out for this workspace and where their sources are on disk. Read those files directly; they are real paths. Read-only."
    )]
    async fn magent_deps(&self) -> Result<String, String> {
        let workspace_id = self
            .store
            .resolve_workspace_for(&self.workspace_roots[0])
            .map_err(|error| render_error(&error))?
            .workspace_id;

        let dependencies = self
            .store
            .dependencies(workspace_id)
            .map_err(|error| render_error(&error))?
            .iter()
            .map(|dependency| self.report(dependency))
            .collect();

        render(&DepsReport {
            root: self.deps_root.clone(),
            dependencies,
        })
    }

    #[tool(
        description = "Close this session (close_session) or complete the whole run (complete_run). Applies to the run open here; closing a session does not finish the task."
    )]
    async fn magent_finish(
        &self,
        Parameters(input): Parameters<FinishToolInput>,
    ) -> Result<String, String> {
        let (run_id, session_id) = self
            .in_flight(input.run_id, input.session_id)
            .map_err(|error| render_error(&error))?;

        let command = FinishRunCommand {
            operation_id: input.operation_id,
            run_id,
            session_id,
            action: input.action,
            outcome: input.outcome,
        };

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
