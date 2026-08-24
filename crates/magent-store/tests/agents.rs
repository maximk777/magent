//! Tests for recording the agents a session runs.
//!
//! Subagents share their parent's session id by design, so nothing before this
//! writes an `agents` row. These tests exercise the two writers that do:
//! `Store::record_agent` and `Store::mark_agent_returned`.

use magent_core::HarnessKind;
use magent_store::Store;
use rusqlite::OptionalExtension;

/// A store on a fresh temp path, plus a hint already bound to a run and
/// session — built the way `store_contract.rs`'s own fixtures build one, by
/// calling `bind_session` rather than writing rows directly.
struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Store,
    session_id: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("magent.db");
        let store = Store::open(&path).expect("open");

        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir");

        let bound = store
            .bind_session(
                "agent-fixture-hint",
                &root,
                "run subagents",
                HarnessKind::ClaudeCode,
            )
            .expect("bind session");

        Self {
            _dir: dir,
            path,
            store,
            session_id: bound.session_id.to_string(),
        }
    }

    const HINT: &'static str = "agent-fixture-hint";

    fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).expect("raw connection")
    }

    /// Reads back `(session_id, agent_type, started_at, ended_at)` for
    /// `agent_id`, if a row exists.
    fn agent_row(
        &self,
        agent_id: &str,
    ) -> Option<(String, Option<String>, String, Option<String>)> {
        self.raw()
            .query_row(
                "SELECT session_id, agent_type, started_at, ended_at FROM agents WHERE id = ?1",
                [agent_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .expect("query agents")
    }

    fn agent_count(&self) -> i64 {
        self.raw()
            .query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))
            .expect("count agents")
    }
}

#[test]
fn an_agent_is_recorded_once_however_often_it_is_seen() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-1", Some("reviewer"))
        .expect("first sighting");
    let first_seen = fixture
        .agent_row("agent-1")
        .expect("recorded on first sighting")
        .2;

    // A different type on the later sightings: if INSERT OR IGNORE were not
    // protecting the row, this would overwrite agent_type along with
    // started_at, and the assertion below would not catch a swapped bind
    // order or a first-write-loses regression.
    fixture
        .store
        .record_agent(Fixture::HINT, "agent-1", Some("implementer"))
        .expect("second sighting");
    fixture
        .store
        .record_agent(Fixture::HINT, "agent-1", Some("implementer"))
        .expect("third sighting");

    assert_eq!(
        fixture.agent_count(),
        1,
        "the same agent id must not produce more than one row however often it is seen"
    );
    let (_, agent_type, third_seen, _) = fixture.agent_row("agent-1").expect("still recorded");
    assert_eq!(
        first_seen, third_seen,
        "started_at must not move once the agent's first row exists"
    );
    assert_eq!(
        agent_type.as_deref(),
        Some("reviewer"),
        "the first sighting's agent_type must survive later sightings that pass a different type"
    );
}

#[test]
fn an_agent_names_the_session_it_ran_under() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-2", Some("reviewer"))
        .expect("record");

    let (session_id, agent_type, _, _) = fixture
        .agent_row("agent-2")
        .expect("the agent must be recorded");
    assert_eq!(
        session_id, fixture.session_id,
        "the recorded row must name the session its hint resolves to"
    );
    assert_eq!(
        agent_type.as_deref(),
        Some("reviewer"),
        "the recorded row must store the agent_type it was given"
    );
}

#[test]
fn an_agent_recorded_with_no_type_reads_back_as_none() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-untyped", None)
        .expect("record");

    let (_, agent_type, _, _) = fixture
        .agent_row("agent-untyped")
        .expect("the agent must be recorded");
    assert_eq!(
        agent_type, None,
        "a missing agent_type must stay NULL rather than becoming an empty string"
    );
}

#[test]
fn an_agent_of_an_unknown_session_is_not_recorded() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent("no-such-hint", "agent-3", Some("worker"))
        .expect("must not error when the hint carries no session");

    assert_eq!(
        fixture.agent_count(),
        0,
        "an agent whose parent has no session row must not be recorded"
    );
}

#[test]
fn a_blank_agent_id_records_nothing() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "", Some("worker"))
        .expect("must not error on an empty id");
    fixture
        .store
        .record_agent(Fixture::HINT, "   ", Some("worker"))
        .expect("must not error on a whitespace-only id");

    assert_eq!(
        fixture.agent_count(),
        0,
        "a blank agent id must never produce a row, since a legal empty TEXT PRIMARY KEY would merge unrelated agents"
    );
}

#[test]
fn a_blank_agent_id_marks_nothing_returned() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-real", Some("reviewer"))
        .expect("record");

    fixture
        .store
        .mark_agent_returned("")
        .expect("must not error on an empty id");
    fixture
        .store
        .mark_agent_returned("   ")
        .expect("must not error on a whitespace-only id");

    let ended = fixture.agent_row("agent-real").expect("recorded").3;
    assert_eq!(
        ended, None,
        "a blank id must not mark an unrelated real agent returned"
    );
}

#[test]
fn an_agent_is_marked_returned_once() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-4", Some("reviewer"))
        .expect("record");
    fixture
        .store
        .mark_agent_returned("agent-4")
        .expect("first return");

    let ended_first = fixture
        .agent_row("agent-4")
        .expect("recorded")
        .3
        .expect("ended_at must be set after the agent returns");

    fixture
        .store
        .mark_agent_returned("agent-4")
        .expect("second return");
    let ended_second = fixture
        .agent_row("agent-4")
        .expect("recorded")
        .3
        .expect("ended_at must still be set");

    assert_eq!(
        ended_first, ended_second,
        "a repeated SubagentStop must not move ended_at once it is set"
    );
}

#[test]
fn an_agent_that_never_returned_has_no_end() {
    let fixture = Fixture::new();

    fixture
        .store
        .record_agent(Fixture::HINT, "agent-5", Some("reviewer"))
        .expect("record");

    let ended = fixture.agent_row("agent-5").expect("recorded").3;
    assert_eq!(
        ended, None,
        "an agent with no SubagentStop event must have no ended_at"
    );
}
