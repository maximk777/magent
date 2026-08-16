//! Magent's MCP surface.
//!
//! This is the portable half of the integration: instructions, tools and (in
//! later slices) prompts and resources travel to any MCP client, while hooks
//! stay Claude Code specific. The division is by guarantee — hooks fire whether
//! or not the model cooperates, so they own capture; MCP owns everything that
//! needs the model's own knowledge.

use std::{path::PathBuf, sync::Arc};

use magent_core::{
    CheckpointCommand, CheckpointOrigin, Classification, Fact, FinishAction, FinishRunCommand,
    HarnessKind, OperationId, ProposeCommand, RememberCommand, RunId, SessionId, SpecifyCommand,
    StartRunCommand, WorkflowStage,
};
use magent_store::{
    Dependency, FactContext, FactQuery, GroupingProposal, Store, StoreError, dependency_checkout,
};
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

When work follows a written plan, pass spec_change_id and current_task to \
magent_checkpoint. Restored context then names the task in hand rather than \
the prompt that opened the run.

Every mutating call takes an operation_id. Generate a fresh UUID per call, and \
reuse the same one when retrying, so a retry cannot duplicate state.

Magent does not replace native approvals: dangerous actions are still confirmed \
in this harness.";

/// Appended when this workspace looks worth setting up.
///
/// The self-describing part of the design: rather than wait to be asked, the
/// server says what it noticed, in the one place the model reads before it does
/// anything. Only when there is something to say — an instruction that is
/// always present is one the model learns to skip.
fn setup_note(proposal: &GroupingProposal) -> Option<String> {
    let name = proposal.suggested_name.as_ref()?;
    if proposal.already_grouped {
        return None;
    }

    Some(format!(
        "\n\nThis workspace is not grouped: {} checkouts here share {}. \
Memory learned in one of them will not reach the others until they are. Call \
magent_setup to see what it would do, and offer it (suggested name: {name}).",
        proposal.siblings.len(),
        proposal
            .organisation
            .as_deref()
            .unwrap_or("one organisation"),
    ))
}

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

    /// The spec change this run is executing, `add-retry-budget`. Set it once;
    /// later checkpoints that omit it leave it bound.
    #[serde(default)]
    pub spec_change_id: Option<String>,
    /// Repository-relative paths to the proposal and task list.
    #[serde(default)]
    pub spec_paths: Vec<String>,
    /// The task now in hand, as it reads in the list: `2: wire the budget`.
    #[serde(default)]
    pub current_task: Option<String>,
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

/// What the client may supply when proposing a change.
///
/// Deliberately smaller than [`ProposeCommand`]: `what_changes` and
/// `capabilities` carry no `#[serde(default)]` on the domain type, so used
/// directly it would force every call to name an empty list explicitly — and
/// worse, it would make the empty-capabilities-without-`skip_specs` case
/// unreachable through this schema. The client's own JSON-Schema validator
/// would refuse the omission before the call ever reached
/// [`Store::propose`](magent_store::Store::propose), so the actionable
/// `missing_capabilities` code the domain layer exists to produce would never
/// surface. Both lists default to empty here instead, leaving that refusal to
/// the domain layer, which can explain it. `impact` and `skip_specs` already
/// default on [`ProposeCommand`] and are carried through unchanged.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ProposeToolInput {
    /// Idempotency key. Reuse it when retrying the same call.
    pub operation_id: OperationId,
    /// The change's address: lowercase letters, digits and single interior
    /// hyphens, `add-retry-budget`-style.
    pub slug: String,
    /// One line, readable on its own in a list of open changes.
    pub title: String,
    /// How much this change is expected to cost, and thus how much process
    /// it owes. `bounded` is the common case.
    pub classification: Classification,
    /// Why this change is worth making, for a reviewer judging whether it
    /// should happen at all.
    pub why: String,
    /// The change in outline, one entry per notable edit.
    #[serde(default)]
    pub what_changes: Vec<String>,
    /// Paths of the capabilities this change touches, `worker/retry`-style.
    /// Leave empty only when `skip_specs` is set.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// What could go wrong, or who else is affected.
    #[serde(default)]
    pub impact: Option<String>,
    /// Set when this change legitimately touches no capability — a pure
    /// refactor, tooling, docs.
    #[serde(default)]
    pub skip_specs: bool,
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

/// What setup was asked to do.
///
/// No `operation_id`: grouping is idempotent on the workspace name, so a retry
/// lands on the same group and an idempotency key would be ceremony that can
/// only be got wrong.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct SetupToolInput {
    /// Look only. Applying regroups repositories, so it is never the default.
    #[serde(default)]
    pub apply: bool,
    /// Overrides the suggested name. Only meaningful with `apply`.
    #[serde(default)]
    pub name: Option<String>,
}

/// The name to group under, confirmed by the person.
///
/// A form rather than a yes/no: the suggested name is a guess from a directory
/// or an organisation, and the one person who knows what this group is called
/// is the one being asked.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct GroupConfirmation {
    /// What to call this workspace.
    pub name: String,
}

rmcp::elicit_safe!(GroupConfirmation);

#[derive(Debug, Serialize)]
struct SetupReport {
    /// A sentence to say out loud. Everything below is the detail behind it.
    summary: String,
    root: PathBuf,
    organisation: Option<String>,
    suggested_name: Option<String>,
    /// The other reasonable name, when the directory and the organisation
    /// disagree. Worth offering rather than deciding.
    alternative_name: Option<String>,
    siblings: Vec<PathBuf>,
    already_grouped: bool,
    workspace_name: Option<String>,
    /// Set once something was actually done.
    grouped: Option<usize>,
    /// The terminal equivalent, so a person can do it themselves.
    command: Option<String>,
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
    ///
    /// # Errors
    ///
    /// Returns whatever [`Store::resolve_workspace_for`] returns, chiefly a
    /// database error. Resolution creates a workspace on first sight of any
    /// directory, so a caller reaching `Err` here has a real failure to act
    /// on, not an unregistered workspace — swallowing it into
    /// `workspace_id: None` would report the latter and hide the former.
    /// The error comes back already rendered. Every caller is a tool body
    /// that would render it the same way, and there is only one failure to
    /// render; four copies of that decision is four places for the fifth to
    /// diverge.
    fn fact_context(&self) -> Result<FactContext, String> {
        let root = &self.workspace_roots[0];
        let workspace_id = self
            .store
            .resolve_workspace_for(root)
            .map_err(|error| render_error(&error))?
            .workspace_id;
        Ok(FactContext {
            workspace_id: Some(workspace_id),
            run_id: None,
            namespace: root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            ..FactContext::default()
        })
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

        let saved = self
            .store
            .save_checkpoint(&command)
            .map_err(|error| render_error(&error))?;

        // After the checkpoint, so a rejected checkpoint cannot leave the run
        // pointing at a task nobody recorded reaching.
        if input.spec_change_id.is_some()
            || !input.spec_paths.is_empty()
            || input.current_task.is_some()
        {
            self.store
                .bind_spec(
                    run_id,
                    &magent_core::SpecBinding {
                        change_id: input.spec_change_id,
                        paths: input.spec_paths,
                        current_task: input.current_task,
                    },
                )
                .map_err(|error| render_error(&error))?;
        }

        render(&saved)
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
        let context = self.fact_context()?;
        let fact = self
            .store
            .recall(&input.name, &context)
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
        let context = self.fact_context()?;
        let fact_id = self
            .store
            .remember(&command, &context)
            .map_err(|error| render_error(&error))?;

        render(&serde_json::json!({ "fact_id": fact_id }))
    }

    #[tool(
        description = "Inspect this workspace and report what is not set up — chiefly checkouts that belong together but are not grouped, so memory learned in one never reaches the others. Read-only unless apply is true, which asks the person to confirm first."
    )]
    async fn magent_setup(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(input): Parameters<SetupToolInput>,
    ) -> Result<String, String> {
        let root = &self.workspace_roots[0];
        let proposal = self
            .store
            .propose_grouping(root)
            .map_err(|error| render_error(&error))?;

        let siblings: Vec<PathBuf> = proposal
            .siblings
            .iter()
            .map(|sibling| sibling.root.clone())
            .collect();

        let command = proposal.suggested_name.as_ref().map(|name| {
            let paths = siblings
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            format!("magent workspace group --name {name} {paths}")
        });

        let mut report = SetupReport {
            summary: summarise(&proposal),
            root: proposal.root.clone(),
            organisation: proposal.organisation.clone(),
            suggested_name: proposal.suggested_name.clone(),
            alternative_name: alternative_name(&proposal),
            siblings: siblings.clone(),
            already_grouped: proposal.already_grouped,
            workspace_name: proposal.workspace_name.clone(),
            grouped: None,
            command: command.clone(),
        };

        if !input.apply {
            return render(&report);
        }

        let Some(suggested) = proposal.suggested_name.clone() else {
            return Err(fail("nothing_to_set_up", "there is nothing here to group"));
        };

        // Regrouping a person's repositories is theirs to decide. Without a way
        // to ask them, the honest answer is to say what to run — an agent
        // deciding this on their behalf is exactly the failure this tool would
        // otherwise introduce.
        if peer.supported_elicitation_modes().is_empty() {
            return Err(fail(
                "cannot_ask",
                &format!(
                    "this client cannot show a confirmation, and grouping {} repositories is not mine to decide. Ask the person to run: {}",
                    siblings.len(),
                    command.unwrap_or_default()
                ),
            ));
        }

        let asked = peer
            .elicit::<GroupConfirmation>(format!(
                "Group {} checkouts of {} into one workspace? What is learned in any of them will then reach all of them.",
                siblings.len(),
                proposal.organisation.as_deref().unwrap_or("this project"),
            ))
            .await;

        let name = match asked {
            Ok(Some(confirmation)) => confirmation.name,
            Ok(None) => input.name.unwrap_or(suggested),
            Err(rmcp::service::ElicitationError::UserDeclined) => {
                return Err(fail("declined", "the person declined; nothing was changed"));
            }
            Err(rmcp::service::ElicitationError::UserCancelled) => {
                return Err(fail(
                    "cancelled",
                    "the request was dismissed; nothing was changed",
                ));
            }
            Err(error) => return Err(fail("could_not_ask", &error.to_string())),
        };

        let name = name.trim();
        if name.is_empty() {
            return Err(fail("empty_name", "a workspace needs a name"));
        }

        let grouped = self
            .store
            .group_into_workspace(name, &siblings)
            .map_err(|error| render_error(&error))?;

        report.grouped = Some(grouped.repositories);
        report.workspace_name = Some(name.to_owned());
        report.already_grouped = true;
        report.summary = format!(
            "{} repositories now share the workspace {name}.",
            grouped.repositories
        );
        render(&report)
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

    #[tool(
        description = "Open a spec-driven change: a proposal plus its process metadata. Requires slug, title, classification and why; capabilities may be left empty only when skip_specs is true, in which case the change writes no deltas."
    )]
    async fn magent_propose(
        &self,
        Parameters(input): Parameters<ProposeToolInput>,
    ) -> Result<String, String> {
        let command = ProposeCommand {
            operation_id: input.operation_id,
            slug: input.slug,
            title: input.title,
            classification: input.classification,
            why: input.why,
            what_changes: input.what_changes,
            capabilities: input.capabilities,
            impact: input.impact,
            skip_specs: input.skip_specs,
        };

        let context = self.fact_context()?;
        let change_id = self
            .store
            .propose(&command, &context)
            .map_err(|error| render_error(&error))?;

        render(&serde_json::json!({ "change_id": change_id }))
    }

    /// Takes the domain command directly, where `magent_propose` needed a
    /// wrapper. The rule behind both, for whoever adds the next tool: wrap
    /// only when a field the domain layer wants to refuse would otherwise be
    /// refused earlier, by the schema, in words the caller cannot act on.
    /// Here nothing qualifies — `purpose` already carries `#[serde(default)]`
    /// and every other field is genuinely required — so a wrapper would add a
    /// second place to forget a field and buy nothing.
    #[tool(
        description = "Attach one capability's requirement deltas to an open change proposed with magent_propose, moving it to specified. Call again for another capability, or to add more requirements to the same one; nothing already attached is replaced. purpose is required only when the capability is new."
    )]
    async fn magent_specify(
        &self,
        Parameters(command): Parameters<SpecifyCommand>,
    ) -> Result<String, String> {
        let context = self.fact_context()?;
        let report = self
            .store
            .specify(&command, &context)
            .map_err(|error| render_error(&error))?;

        render(&report)
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
        // Built per connection rather than fixed, so the server can say what
        // it found here. Truncated by the client at 2 KB, so the note is
        // appended only when it earns the bytes.
        let mut instructions = INSTRUCTIONS.to_owned();
        if let Ok(proposal) = self.store.propose_grouping(&self.workspace_roots[0])
            && let Some(note) = setup_note(&proposal)
        {
            instructions.push_str(&note);
        }
        info.instructions = Some(instructions);
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
/// A tool failure in the same shape store failures take, so a caller parses
/// one thing.
fn fail(code: &str, message: &str) -> String {
    serde_json::json!({ "code": code, "message": message }).to_string()
}

/// The sentence a person should hear.
fn summarise(proposal: &GroupingProposal) -> String {
    if proposal.already_grouped {
        return proposal.workspace_name.as_ref().map_or_else(
            || "This workspace is already grouped.".to_owned(),
            |name| format!("Already grouped as {name}; nothing to set up."),
        );
    }

    match (&proposal.suggested_name, &proposal.organisation) {
        (Some(name), Some(organisation)) => format!(
            "{} checkouts here share {organisation} but are not grouped, so what is learned in one does not reach the others. Grouping them as {name} would fix that.",
            proposal.siblings.len(),
        ),
        _ => "Nothing here needs setting up: this is a single repository, and it is registered already.".to_owned(),
    }
}

/// The name not suggested, when the directory and the organisation disagree.
fn alternative_name(proposal: &GroupingProposal) -> Option<String> {
    let suggested = proposal.suggested_name.as_ref()?;
    let organisation = proposal.organisation.as_ref()?;
    let from_origin = organisation.rsplit('/').next()?;

    (from_origin != suggested).then(|| from_origin.to_owned())
}

fn render_error(error: &StoreError) -> String {
    serde_json::json!({
        "code": error.code(),
        "message": error.to_string(),
    })
    .to_string()
}
