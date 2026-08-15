//! Contracts for the memory layer.
//!
//! A fact is not a note. It carries where it applies, how it can conflict with
//! another fact, how much it should be trusted, and what it was learned from.
//! Those four things are what let memory be queried instead of merely stored,
//! and they are what these tests pin.

use magent_core::{
    Cardinality, Evidence, FactId, FactKind, FactScope, FactStatus, RelationKind, RememberCommand,
    Validate,
};

fn valid_remember() -> RememberCommand {
    RememberCommand {
        operation_id: magent_core::OperationId::new(),
        name: "goose-table-locking".into(),
        title: "goose v3.26 locks with NewPostgresTableLocker".into(),
        body: "Use lock.NewPostgresTableLocker(...) plus goose.WithLocker(locker).".into(),
        kind: FactKind::Project,
        scope: FactScope::Repository,
        cardinality: Cardinality::Single,
        status: FactStatus::Observed,
        confidence: 0.8,
        evidence: vec![Evidence {
            locator: "internal/migrate/migrate.go:41".into(),
            excerpt: Some("goose.WithLocker(locker)".into()),
        }],
        relates_to: vec![("goose-migrate-arm-testbed".into(), RelationKind::Related)],
    }
}

// --- validation ------------------------------------------------------------

/// The name is how a fact is addressed and superseded. Without one there is no
/// way to say "this replaces that".
#[test]
fn a_fact_must_be_named() {
    let command = RememberCommand {
        name: "   ".into(),
        ..valid_remember()
    };

    assert_eq!(command.validate().unwrap_err().code(), "invalid_fact_name");
}

/// Names are addresses, so they must survive a round trip through a filename,
/// a wikilink and a URL unchanged.
#[test]
fn a_fact_name_must_be_a_slug() {
    for rejected in ["Not A Slug", "has/slash", "trailing-", "UPPER"] {
        let command = RememberCommand {
            name: rejected.into(),
            ..valid_remember()
        };
        assert_eq!(
            command.validate().unwrap_err().code(),
            "invalid_fact_name",
            "{rejected} should not be a valid name"
        );
    }

    for accepted in ["goose-table-locking", "op401", "a", "x-1-y"] {
        let command = RememberCommand {
            name: accepted.into(),
            ..valid_remember()
        };
        assert!(
            command.validate().is_ok(),
            "{accepted} should be a valid name"
        );
    }
}

/// A fact with no statement is a filing error, not knowledge.
#[test]
fn a_fact_must_say_something() {
    let command = RememberCommand {
        title: " ".into(),
        body: " ".into(),
        ..valid_remember()
    };

    assert_eq!(command.validate().unwrap_err().code(), "invalid_fact_body");
}

/// Confidence outside 0..=1 makes ranking meaningless, and a caller that emits
/// it is confused about the scale.
#[test]
fn confidence_must_be_a_probability() {
    for rejected in [-0.1, 1.5, f64::NAN] {
        let command = RememberCommand {
            confidence: rejected,
            ..valid_remember()
        };
        assert_eq!(
            command.validate().unwrap_err().code(),
            "invalid_confidence",
            "{rejected} should be rejected"
        );
    }
}

/// `verified` is a claim about evidence. Allowing it without any would let the
/// strongest status be the cheapest one to assert.
#[test]
fn verified_requires_evidence() {
    let command = RememberCommand {
        status: FactStatus::Verified,
        evidence: vec![],
        ..valid_remember()
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "verified_without_evidence"
    );
}

#[test]
fn a_well_formed_fact_is_accepted() {
    assert!(valid_remember().validate().is_ok());
}

// --- wire shape ------------------------------------------------------------

#[test]
fn enums_use_their_snake_case_names() {
    let cases: Vec<(String, &str)> = vec![
        (
            serde_json::to_string(&FactScope::Repository).unwrap(),
            "\"repository\"",
        ),
        (
            serde_json::to_string(&FactKind::Feedback).unwrap(),
            "\"feedback\"",
        ),
        (
            serde_json::to_string(&Cardinality::Timeline).unwrap(),
            "\"timeline\"",
        ),
        (
            serde_json::to_string(&FactStatus::Contradicted).unwrap(),
            "\"contradicted\"",
        ),
        (
            serde_json::to_string(&RelationKind::Supersedes).unwrap(),
            "\"supersedes\"",
        ),
    ];

    for (actual, expected) in cases {
        assert_eq!(actual, expected);
    }
}

#[test]
fn a_remember_command_round_trips() {
    let command = valid_remember();
    let encoded = serde_json::to_string(&command).expect("serialize");
    let decoded: RememberCommand = serde_json::from_str(&encoded).expect("deserialize");

    assert_eq!(decoded, command);
}

#[test]
fn fact_ids_serialize_as_bare_strings() {
    let encoded = serde_json::to_string(&FactId::new()).expect("serialize");
    assert!(encoded.starts_with('"'), "{encoded}");
    assert_eq!(encoded.trim_matches('"').len(), 36);
}

// --- cardinality semantics -------------------------------------------------

/// Cardinality is what tells the store whether a new value replaces an old one,
/// sits beside it, or covers a different stretch of time. Getting it wrong
/// either loses knowledge or piles up contradictions.
#[test]
fn cardinality_states_how_two_values_may_coexist() {
    assert!(
        Cardinality::Single.conflicts_within_scope(),
        "one current value per scope, so a new one supersedes"
    );
    assert!(
        !Cardinality::Set.conflicts_within_scope(),
        "distinct values coexist"
    );
    assert!(
        Cardinality::Timeline.conflicts_within_scope(),
        "values conflict only where their intervals overlap"
    );
}

/// Scopes are ordered from broadest to narrowest, so a repository-specific fact
/// can outrank a general one about the same thing.
#[test]
fn scopes_are_ordered_from_general_to_specific() {
    assert!(FactScope::User < FactScope::Workspace);
    assert!(FactScope::Workspace < FactScope::Repository);
    assert!(FactScope::Repository < FactScope::Run);
}
