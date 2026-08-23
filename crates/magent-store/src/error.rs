use magent_core::{ChangeId, DependencyId, DomainError, OperationId, RunId, SessionId};
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Domain(#[from] DomainError),

    #[error("database error: {0}")]
    Database(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("run {0} does not exist")]
    RunNotFound(RunId),

    #[error("session {0} does not exist")]
    SessionNotFound(SessionId),

    #[error("run {0} is already completed")]
    RunClosed(RunId),

    /// The same `operation_id` arrived carrying a different request. Replaying
    /// the stored response would answer a question that was never asked.
    #[error("operation {0} was already used for a different request")]
    IdempotencyConflict(OperationId),

    #[error("database schema version {0} is newer than this build understands")]
    UnsupportedSchema(i64),

    /// A tool that works on "the run in flight" was called with nothing in
    /// flight. Opening one here would produce a run with no task.
    #[error("no run is open in this workspace; call magent_start first")]
    NoOpenRun,

    #[error("dependency {0} does not exist")]
    DependencyNotFound(DependencyId),

    /// The slug names a change that is already past its proposal. A change
    /// still `drafting` or `specified` has its proposal rewritten instead, so
    /// arriving here means the proposal has already produced a plan — and
    /// moving the agreement out from under work already broken down is not a
    /// correction but a substitution.
    ///
    /// The unique index on live slugs (`sdd_changes_live_slug`) would catch a
    /// second insert too, but "UNIQUE constraint failed" does not tell a
    /// caller what to do about it. Checked explicitly so the message does.
    #[error(
        "slug {0:?} belongs to a change that is already past its proposal; \
         a proposal is rewritten only while the change is drafting or specified"
    )]
    SlugTaken(String),

    /// A rewritten proposal stopped declaring a capability this change has
    /// already written deltas for. Accepting it would leave those deltas
    /// filed under nothing the proposal agrees to — written work lost without
    /// a word, which this store treats as worse than a refusal.
    ///
    /// The way out is to keep the capability in `capabilities`: there is no
    /// verb that withdraws a delta, so the deltas cannot be cleared first.
    #[error(
        "this change already proposes deltas for capabilities this proposal drops: {}; \
         keep them in capabilities, since a delta once written cannot be withdrawn",
        .0.join(", ")
    )]
    CapabilityDeltasStranded(Vec<String>),

    /// `OpenSpec` names the proposal's Capabilities section the contract
    /// between the proposal and the specs written against it. Nothing in the
    /// schema relates `spec_deltas.capability_path` to that list, so a spec
    /// filed against a capability nobody proposed would be accepted and never
    /// noticed. The declared paths are carried rather than left implied: a
    /// caller told only that this one is wrong has to go and read the
    /// proposal out of the database to find out which one is right.
    #[error("capability {capability_path:?} is not one this change proposed; {}", declared_detail(.declared))]
    CapabilityNotProposed {
        capability_path: String,
        declared: Vec<String>,
    },

    /// A change belongs to a workspace, and the caller's context did not name
    /// one. The column is `NOT NULL`, so the database would refuse this
    /// anyway — with a message about a constraint rather than about the
    /// context, which is where the answer actually is.
    ///
    /// The message names the fact and stops there. `resolve_workspace_for`
    /// creates a workspace on first sight of any directory, so the only way
    /// to arrive here is a resolution that failed and was discarded upstream;
    /// no tool the caller could run would change that, and telling it to run
    /// one would send it somewhere the fix is not.
    #[error("this context names no workspace to file the change under")]
    NoWorkspace,

    /// A delta was offered for a change that is not in this workspace. The
    /// foreign key on `spec_deltas.change_id` would catch a missing id, but it
    /// would say "FOREIGN KEY constraint failed" — which does not tell a
    /// caller whether it mistyped an id, or is holding one from a workspace it
    /// has since moved out of.
    #[error("change {0} does not exist in this workspace")]
    ChangeNotFound(ChangeId),

    /// Archived and abandoned changes are finished. Writing a delta onto one
    /// would edit a record of what was decided, and no constraint stops it:
    /// the status column is happy either way, so the refusal has to be here.
    #[error("change {0} is archived or abandoned and takes no further specs")]
    ChangeClosed(ChangeId),

    /// A delta naming a capability that does not exist yet is how a new
    /// capability is proposed, and a capability with no stated purpose is one
    /// nobody can review. `magent-core` checks that a purpose, if given, is
    /// long enough; only the store can see that this one is new and therefore
    /// owes one.
    #[error("capability {0:?} is new and needs a purpose")]
    CapabilityPurposeRequired(String),

    /// `OpenSpec` accepts a purpose for a capability that already has one and
    /// drops it — their own instructions admit as much. Silently discarding
    /// text a person wrote is worse than refusing it: the author believes it
    /// was recorded and never learns otherwise.
    #[error(
        "capability {0:?} already has a purpose; remove it from this delta or edit the capability"
    )]
    CapabilityPurposeRedundant(String),

    /// Nothing outside the store addresses a requirement by id: a patch delta
    /// names one, and `specify` resolves the id from the capability and that
    /// name. So this is never a mistyped address. It is raised only on the
    /// archive path, and usually against an id the store itself wrote, where
    /// what it reports is that the row moved underneath that id — `specify`
    /// found the requirement live, and between then and `archive` another
    /// change retired it. The one exception carries no id at all — a patch
    /// delta that reached `archive` without one, which `specify` should have
    /// made impossible — and it reports that in place of an id rather than
    /// being passed over in silence.
    ///
    /// The guarded `UPDATE` is what notices: it matches nothing where an
    /// unguarded one would rewrite a retired requirement with text nobody
    /// agreed to ship. The message says *live* rather than *missing* because a
    /// retired requirement is kept rather than deleted — the row is still
    /// there, and calling it absent would send a reader looking for the wrong
    /// thing.
    #[error(
        "requirement {requirement_id} is not a live requirement of capability {capability_path:?}"
    )]
    RequirementNotFound {
        requirement_id: String,
        capability_path: String,
    },

    /// A patch delta named a requirement that is not live in the capability it
    /// was filed under. The live names come with it: a caller that named
    /// something it cannot find is handed the list it should have named from,
    /// which is the rule the change lookup already follows.
    #[error("no live requirement of capability {capability_path:?} is called {name:?}; it holds: {}", .live.join(", "))]
    RequirementNameNotLive {
        name: String,
        capability_path: String,
        live: Vec<String>,
    },

    /// A change accumulates deltas: a second `specify` for the same capability
    /// adds to what is already proposed rather than replacing it, so a
    /// requirement name it has used once cannot be used again. The
    /// `spec_deltas_identity` index says so too, and says it as "UNIQUE
    /// constraint failed" — the message this method's other checks exist to
    /// keep a caller from having to interpret.
    #[error(
        "this change already proposes a requirement named {requirement_name:?} for capability {capability_path:?}"
    )]
    DeltaAlreadyProposed {
        requirement_name: String,
        capability_path: String,
    },

    /// A plan describes how to satisfy a spec, so there has to be one to plan
    /// against. The status column takes any value the process puts there and
    /// no constraint relates it to whether deltas exist, so the refusal has to
    /// be here. The status is carried rather than assumed, because "specify it
    /// first" is the wrong advice for a change that is already past planning.
    #[error(
        "change {change} is {status:?}; a plan is written from a specified change, or from a drafting one proposed with skip_specs"
    )]
    ChangeNotSpecified { change: ChangeId, status: String },

    /// Every requirement a change proposes needs a task that implements it,
    /// and the names are the part of that answer the caller cannot work out
    /// from the command it just sent. Listed rather than counted: "coverage is
    /// incomplete" makes the caller re-derive the query against a plan it has
    /// only been told is wrong.
    #[error(
        "no task covers these requirements: {}; add a task for each before planning",
        .0.join(", ")
    )]
    RequirementsUncovered(Vec<String>),

    /// The same question asked at the other end of the process, and a separate
    /// variant because the answer means something different. `RequirementsUncovered`
    /// says the plan in hand is incomplete; this says the plan is fine and the
    /// work is not — every task exists, and the one covering this requirement
    /// was never finished. Sharing one code would leave the caller unable to
    /// tell "write another task" from "close the one you have", and the older
    /// message would be false outright for a task that was skipped, where a
    /// covering task demonstrably exists.
    #[error(
        "no finished task covers these requirements: {}; close the task covering each, or plan one, before archiving",
        .0.join(", ")
    )]
    RequirementsUnimplemented(Vec<String>),

    /// A plan whose `consumes` names an artifact no task in it produces.
    ///
    /// Carries the entries rather than a count. An executing agent sees its own
    /// task and nothing around it, so a name it was told to build on and cannot
    /// find leaves it nothing to fall back on but a guess — and it will guess
    /// plausibly. The refusal has to say which promise is missing.
    ///
    /// Matched by exact equality after trimming: `superpowers` states the rule
    /// about repeating the exact name in prose, where it goes unenforced, and
    /// this is that rule as a check. A near-miss is a miss.
    #[error(
        "these consumed artifacts are produced by no task in the plan: {}; produce each, or correct the name to match the task that does",
        .0.join(", ")
    )]
    ArtifactsUnproduced(Vec<String>),

    /// A plan whose dependencies form a cycle.
    ///
    /// Not a plan at all: no task in it can ever be first. Carries the tasks
    /// left over when nothing more could be ordered, which is the cycle.
    #[error(
        "these tasks depend on each other in a cycle: {}; no order can satisfy them, so one must stop consuming what a later task produces",
        .0.join(", ")
    )]
    PlanIsCyclic(Vec<String>),

    /// Archiving files a change's deltas as what is now true of the product,
    /// so work still open would put an unbuilt behaviour into the live base
    /// and every later change would read it as the starting point. Nothing in
    /// the schema relates a change's status to its tasks', so the refusal has
    /// to be here.
    ///
    /// The numbers are listed rather than counted, for the same reason
    /// [`StoreError::RequirementsUncovered`] lists names: they are the part of
    /// the answer the caller cannot derive from the command it just sent. An
    /// empty list is the other shape of the same fault — a change with specs
    /// and no plan at all was not executed either, and saying so in this
    /// error keeps "nothing did this work" one answer instead of two.
    #[error("change {change} is not executed: {}", unexecuted_detail(.tasks))]
    ChangeNotExecuted {
        change: ChangeId,
        tasks: Vec<String>,
    },

    /// Archiving is the step that makes a change's deltas true; a change
    /// carrying none has nothing to fold in. Moving it to `archived` anyway
    /// would be the quietest possible way to lose it — the status would say
    /// the work landed, and the live base would be untouched. Only a change
    /// proposed with `skip_specs` may legitimately arrive here empty.
    #[error(
        "change {0} proposes no spec deltas and did not declare skip_specs; there is nothing to archive"
    )]
    NothingToArchive(ChangeId),

    /// A checkpoint carried a tick, and the run it came on names no change.
    /// The slug a task number is looked up in comes off the run rather than off
    /// the tick — a checkpoint late in a task carries the number and nothing
    /// else — so a run bound to nothing leaves no plan the number could belong
    /// to. Refused first, because every refusal after it is a statement about a
    /// plan.
    #[error(
        "run {run} is bound to no change, so there is no plan to look a task number up in; \
         bind it to the change being executed and send the tick again"
    )]
    RunNotBoundToChange { run: RunId },

    /// The slug the run is bound to names an open change in more than one
    /// namespace. `sdd::change_by_slug` hands back every match and refuses
    /// nothing, because only its caller knows what was being resolved; here it
    /// was a tick, and closing whichever change sorted first would file the
    /// evidence of finished work against a plan nobody did it for.
    ///
    /// The namespaces rather than the ids: the ids are UUIDs the caller has
    /// never seen, and the namespace is the one part of this a person can act
    /// on.
    #[error(
        "the slug {slug:?} this run is bound to names an open change in each of {}; \
         nothing can tell which of those plans the tick belongs to",
        .namespaces.join(", ")
    )]
    ChangeSlugAmbiguous {
        slug: String,
        namespaces: Vec<String>,
    },

    /// The run is bound to a slug that no open change here answers to — the
    /// change has been archived or abandoned since, or the binding names a
    /// change of another workspace. Distinct from
    /// [`StoreError::ChangeNotFound`], which carries a `ChangeId`: a run row
    /// holds a slug, and telling a caller that some uuid is missing would name
    /// something it never supplied.
    ///
    /// The two causes have different repairs, so the message carries them the
    /// way [`StoreError::ChangeNotSpecified`] does rather than leaving them in a
    /// doc comment nothing reads at the moment of the refusal.
    #[error(
        "no open change in this workspace is called {0:?}; a slug leaves this list once its \
         change is archived or abandoned, so re-bind the run to the change being executed — \
         or, if that change is the archived one, its work is already signed off"
    )]
    ChangeSlugNotFound(String),

    /// A tick named a number this plan has no task for. The numbers still open
    /// travel with it for the reason [`StoreError::ChangeNotExecuted`] and
    /// [`StoreError::CapabilityNotProposed`] carry their lists: they are the
    /// part of the answer the caller cannot derive from the command it just
    /// sent, and without them the only way on is a query against a plan the
    /// caller has been told nothing about except that its number is wrong.
    ///
    /// The plan is named for a fault the numbers alone would misdiagnose: a run
    /// left bound to one change while its session executes another reaches here
    /// with a perfectly good number, and a message that named no plan would send
    /// the caller to correct the number instead of the binding.
    #[error(
        "the plan of {slug:?} has no task numbered {number:?}; {}",
        open_detail(.slug, .open)
    )]
    TaskNotFound {
        slug: String,
        number: String,
        open: Vec<String>,
    },

    /// The tick carried a different command from the one the plan named for
    /// this task. Compared on the trimmed text and nothing looser: a plan
    /// states its verification precisely so that the box cannot be ticked by
    /// running something else, and a comparison that accepted a near miss would
    /// hand back the hole it closes — the evidence would be of some other
    /// command's output.
    ///
    /// The planned command travels because it is the thing to do instead, and
    /// the caller would otherwise have to read the plan to find it.
    #[error(
        "task {number} is verified by {expected:?}; a tick carrying another command \
         proves nothing about it"
    )]
    VerifyCommandMismatch { number: String, expected: String },

    /// A task edited a file another session was holding for its own task.
    ///
    /// Late by construction: the hook sees an edit only after the tool has made
    /// it, so nothing can stop the edit itself. What closing time can do is
    /// refuse to file a collision as a clean piece of work, and make somebody
    /// look at it before the plan reads as tidy.
    ///
    /// The other task's number travels because it is who to go and talk to, and
    /// the path because a refusal naming only a count is one nobody can act on.
    #[error(
        "task {number} edited {path}, which task {holder} was holding at the time; \
         closing it would file a collision as clean work"
    )]
    FileHeldByAnotherTask {
        number: String,
        path: String,
        holder: String,
    },
}

/// The tail of [`StoreError::ChangeNotExecuted`]'s message.
///
/// Two shapes rather than one: a list of numbers reads as work in progress,
/// and an empty list means there was never a plan, which is a different thing
/// for the caller to fix even though it is the same refusal.
/// The tail of [`StoreError::CapabilityNotProposed`]'s message.
///
/// Two shapes for the same reason `unexecuted_detail` has two: an empty list
/// is what a change proposed with `skip_specs` looks like, and "the proposal
/// declares: " trailing off into nothing reads as a bug in the message rather
/// than as the fact that there is nothing to declare against.
fn declared_detail(declared: &[String]) -> String {
    if declared.is_empty() {
        "its proposal declares no capabilities at all".to_owned()
    } else {
        format!("its proposal declares: {}", declared.join(", "))
    }
}

/// The tail of [`StoreError::TaskNotFound`]'s message.
///
/// Two shapes, as `unexecuted_detail` has: an empty list means every task of
/// the plan is already closed, so "the numbers still open are: " trailing off
/// into nothing would read as a bug in the message rather than as the answer —
/// which here is that the mistake is in the number, not in the plan's progress.
///
/// The plan is named in both, never left as "it": a caller holding a stale
/// binding is reading this message about a plan other than the one it is
/// working on, which is precisely the case an anonymous pronoun hides.
fn open_detail(slug: &str, open: &[String]) -> String {
    if open.is_empty() {
        format!("every task of {slug:?} is already closed")
    } else {
        format!(
            "the numbers still open in {slug:?} are: {}",
            open.join(", ")
        )
    }
}

fn unexecuted_detail(tasks: &[String]) -> String {
    if tasks.is_empty() {
        "it has no tasks at all, so nothing on it was planned or done".to_owned()
    } else {
        format!("these tasks are still open: {}", tasks.join(", "))
    }
}

impl StoreError {
    /// Stable `snake_case` identifier, surfaced to the model through MCP.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Domain(inner) => inner.code(),
            Self::Database(_) => "database_error",
            Self::Serialization(_) => "serialization_error",
            Self::RunNotFound(_) => "run_not_found",
            Self::SessionNotFound(_) => "session_not_found",
            Self::RunClosed(_) => "run_closed",
            Self::IdempotencyConflict(_) => "idempotency_conflict",
            Self::UnsupportedSchema(_) => "unsupported_schema",
            Self::NoOpenRun => "no_open_run",
            Self::DependencyNotFound(_) => "dependency_not_found",
            Self::SlugTaken(_) => "slug_taken",
            Self::CapabilityDeltasStranded(_) => "capability_deltas_stranded",
            Self::CapabilityNotProposed { .. } => "capability_not_proposed",
            Self::NoWorkspace => "no_workspace",
            Self::ChangeNotFound(_) => "change_not_found",
            Self::ChangeClosed(_) => "change_closed",
            Self::CapabilityPurposeRequired(_) => "capability_purpose_required",
            Self::CapabilityPurposeRedundant(_) => "capability_purpose_redundant",
            Self::RequirementNotFound { .. } => "requirement_not_found",
            Self::RequirementNameNotLive { .. } => "requirement_name_not_live",
            Self::DeltaAlreadyProposed { .. } => "delta_already_proposed",
            Self::ChangeNotSpecified { .. } => "change_not_specified",
            Self::RequirementsUncovered(_) => "requirements_uncovered",
            Self::RequirementsUnimplemented(_) => "requirements_unimplemented",
            Self::ArtifactsUnproduced(_) => "artifacts_unproduced",
            Self::PlanIsCyclic(_) => "plan_is_cyclic",
            Self::ChangeNotExecuted { .. } => "change_not_executed",
            Self::NothingToArchive(_) => "nothing_to_archive",
            Self::RunNotBoundToChange { .. } => "run_not_bound",
            Self::ChangeSlugAmbiguous { .. } => "change_slug_ambiguous",
            Self::ChangeSlugNotFound(_) => "change_slug_not_found",
            Self::TaskNotFound { .. } => "task_not_found",
            Self::VerifyCommandMismatch { .. } => "verify_command_mismatch",
            Self::FileHeldByAnotherTask { .. } => "file_held_by_another_task",
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error.to_string())
    }
}
