//! Contracts for the spec-driven process.
//!
//! `magent-core` knows nothing about what is already in the database, so what
//! it checks here is shape only: a required field is present, a list is not
//! empty, a string is not blank, names within one command do not repeat.
//! Existence checks — is this capability real, is this slug taken, does this
//! change already have a proposal — belong to the store.

use magent_core::{
    ArchiveCommand, Classification, DeltaOp, PlanCommand, ProposeCommand, RequirementDraft,
    ScenarioDraft, SpecifyCommand, TaskDraft, Validate,
};

fn valid_propose() -> ProposeCommand {
    ProposeCommand {
        operation_id: magent_core::OperationId::new(),
        slug: "add-retry-budget".into(),
        title: "Add a retry budget".into(),
        classification: Classification::Bounded,
        why: "Retries currently loop forever and starve the worker pool.".into(),
        what_changes: vec!["Cap retries at a configurable budget.".into()],
        capabilities: vec!["worker/retry".into()],
        impact: None,
        skip_specs: false,
    }
}

fn valid_scenario() -> ScenarioDraft {
    ScenarioDraft {
        name: "budget exhausted".into(),
        given: Some("a task that has failed three times".into()),
        when: "the retry budget is checked".into(),
        then: "the task is marked failed instead of retried".into(),
    }
}

fn valid_requirement() -> RequirementDraft {
    RequirementDraft {
        op: DeltaOp::Added,
        name: "retry-budget-cap".into(),
        text: Some("The worker MUST cap retries at the configured budget.".into()),
        rename_to: None,
        reason: None,
        migration: None,
        scenarios: vec![valid_scenario()],
    }
}

fn valid_task() -> TaskDraft {
    TaskDraft {
        number: "1.2".into(),
        title: "Add the retry budget field".into(),
        body: Some("Add a `budget: u32` field to `RetryPolicy` and thread it through.".into()),
        files: vec!["crates/worker/src/retry.rs".into()],
        consumes: Vec::new(),
        produces: vec!["RetryPolicy::budget".into()],
        verify_command: "cargo test -p worker retry_budget".into(),
        expected_output: vec!["test retry_budget ... ok".into()],
        covers: vec!["retry-budget-cap".into()],
    }
}

fn valid_plan() -> PlanCommand {
    PlanCommand {
        operation_id: magent_core::OperationId::new(),
        change: magent_core::ChangeId::new(),
        tasks: vec![valid_task()],
        check_only: false,
    }
}

fn valid_archive() -> ArchiveCommand {
    ArchiveCommand {
        operation_id: magent_core::OperationId::new(),
        change: magent_core::ChangeId::new(),
    }
}

fn valid_specify() -> SpecifyCommand {
    SpecifyCommand {
        operation_id: magent_core::OperationId::new(),
        change: magent_core::ChangeId::new(),
        capability_path: "worker/retry".into(),
        purpose: Some(
            "Defines how the worker pool retries failed tasks and when it gives up.".into(),
        ),
        requirements: vec![valid_requirement()],
    }
}

// --- ProposeCommand ----------------------------------------------------

/// A change must either name a capability it touches or explicitly say it
/// carries no spec work. Without the flag, a change could invent a
/// requirement solely to pass this check.
#[test]
fn a_proposal_without_capabilities_must_skip_specs() {
    let command = ProposeCommand {
        capabilities: vec![],
        skip_specs: false,
        ..valid_propose()
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "missing_capabilities"
    );

    let command = ProposeCommand {
        capabilities: vec![],
        skip_specs: true,
        ..valid_propose()
    };
    assert!(command.validate().is_ok());
}

/// The slug is the change's address and must survive being read back as a
/// path segment: lowercase letters, digits and interior hyphens only.
#[test]
fn a_proposal_slug_must_be_kebab_case() {
    for rejected in [
        "",
        "Add-Feature",
        "add_feature",
        "-leading",
        "trailing-",
        "add--retry",
    ] {
        let command = ProposeCommand {
            slug: rejected.into(),
            ..valid_propose()
        };
        assert_eq!(
            command.validate().unwrap_err().code(),
            "invalid_change_slug",
            "{rejected} should not be a valid slug"
        );
    }

    for accepted in ["add-retry-budget", "a", "x-1-y"] {
        let command = ProposeCommand {
            slug: accepted.into(),
            ..valid_propose()
        };
        assert!(
            command.validate().is_ok(),
            "{accepted} should be a valid slug"
        );
    }
}

/// A blank title or rationale is not a proposal, whatever whitespace it is
/// padded with.
#[test]
fn a_proposal_title_and_why_must_not_be_blank() {
    let command = ProposeCommand {
        title: "   ".into(),
        ..valid_propose()
    };
    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_change_title"
    );

    let command = ProposeCommand {
        why: "\t\n".into(),
        ..valid_propose()
    };
    assert_eq!(command.validate().unwrap_err().code(), "invalid_change_why");
}

#[test]
fn a_well_formed_proposal_is_accepted() {
    assert!(valid_propose().validate().is_ok());
}

// --- SpecifyCommand ------------------------------------------------------

/// Specifying a capability with no requirements attached is a no-op that
/// should never have been submitted.
#[test]
fn a_specify_command_must_carry_at_least_one_requirement() {
    let command = SpecifyCommand {
        requirements: vec![],
        ..valid_specify()
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "missing_requirements"
    );
}

/// A purpose shorter than 50 characters is too glib to be useful, the same
/// threshold `OpenSpec`'s own strict validator uses.
#[test]
fn a_specify_purpose_must_meet_the_length_floor() {
    let command = SpecifyCommand {
        purpose: Some("too short".into()),
        ..valid_specify()
    };

    assert_eq!(command.validate().unwrap_err().code(), "invalid_purpose");
}

/// The floor is counted in characters, not bytes. Thirty Cyrillic
/// characters are thirty characters, well under the floor, but sixty bytes
/// in UTF-8 — over it. A byte-counting check would wave this through.
#[test]
fn the_purpose_length_floor_counts_characters_not_bytes() {
    let purpose = "воркер должен ограничивать повторы"
        .chars()
        .take(30)
        .collect::<String>();
    assert_eq!(
        purpose.chars().count(),
        30,
        "fixture must be exactly 30 characters"
    );
    assert!(
        purpose.len() > 50,
        "fixture must exceed 50 bytes for the test to prove anything"
    );

    let command = SpecifyCommand {
        purpose: Some(purpose),
        ..valid_specify()
    };

    assert_eq!(command.validate().unwrap_err().code(), "invalid_purpose");
}

/// Two requirements sharing a name in the same command would collide in the
/// store's unique index; catching it here names exactly what was duplicated.
#[test]
fn requirement_names_must_not_repeat_within_a_specify_command() {
    let command = SpecifyCommand {
        requirements: vec![valid_requirement(), valid_requirement()],
        ..valid_specify()
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "duplicate_requirement_name"
    );
}

// --- RequirementDraft ----------------------------------------------------

/// An ADDED or MODIFIED requirement with no scenarios is exactly the failure
/// `OpenSpec` lets through silently when a scenario is malformed.
#[test]
fn added_or_modified_requirements_need_at_least_one_scenario() {
    for op in [DeltaOp::Added, DeltaOp::Modified] {
        let command = SpecifyCommand {
            requirements: vec![RequirementDraft {
                op,
                scenarios: vec![],
                ..valid_requirement()
            }],
            ..valid_specify()
        };
        assert_eq!(
            command.validate().unwrap_err().code(),
            "missing_scenarios",
            "{op:?} without scenarios should be rejected"
        );
    }
}

/// An ADDED or MODIFIED requirement with no text is a placeholder, not a
/// requirement.
#[test]
fn added_or_modified_requirements_need_text() {
    for op in [DeltaOp::Added, DeltaOp::Modified] {
        let command = SpecifyCommand {
            requirements: vec![RequirementDraft {
                op,
                text: None,
                ..valid_requirement()
            }],
            ..valid_specify()
        };
        assert_eq!(
            command.validate().unwrap_err().code(),
            "missing_requirement_text",
            "{op:?} without text should be rejected"
        );

        let command = SpecifyCommand {
            requirements: vec![RequirementDraft {
                op,
                text: Some("   ".into()),
                ..valid_requirement()
            }],
            ..valid_specify()
        };
        assert_eq!(
            command.validate().unwrap_err().code(),
            "missing_requirement_text",
            "{op:?} with blank text should be rejected"
        );
    }
}

/// A REMOVED requirement without a reason and a migration path is a break
/// nobody explained.
#[test]
fn removed_requirements_need_a_reason_and_a_migration() {
    let base = RequirementDraft {
        op: DeltaOp::Removed,
        text: None,
        scenarios: vec![],
        reason: Some("Superseded by the retry budget.".into()),
        migration: Some("Callers should switch to the budgeted retry helper.".into()),
        ..valid_requirement()
    };

    let command = SpecifyCommand {
        requirements: vec![RequirementDraft {
            reason: None,
            ..base.clone()
        }],
        ..valid_specify()
    };
    assert_eq!(
        command.validate().unwrap_err().code(),
        "missing_removal_reason"
    );

    let command = SpecifyCommand {
        requirements: vec![RequirementDraft {
            migration: None,
            ..base.clone()
        }],
        ..valid_specify()
    };
    assert_eq!(
        command.validate().unwrap_err().code(),
        "missing_removal_migration"
    );

    let command = SpecifyCommand {
        requirements: vec![base],
        ..valid_specify()
    };
    assert!(command.validate().is_ok());
}

/// A RENAMED requirement without a new name has nothing to rename to.
#[test]
fn renamed_requirements_need_a_new_name() {
    let command = SpecifyCommand {
        requirements: vec![RequirementDraft {
            op: DeltaOp::Renamed,
            text: None,
            scenarios: vec![],
            rename_to: None,
            ..valid_requirement()
        }],
        ..valid_specify()
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "missing_rename_target"
    );
}

// --- ScenarioDraft ---------------------------------------------------------

/// `when` and `then` are the scenario's substance; blank ones are not
/// scenarios at all. `given` stays optional.
#[test]
fn scenario_when_and_then_must_not_be_blank() {
    let command = SpecifyCommand {
        requirements: vec![RequirementDraft {
            scenarios: vec![ScenarioDraft {
                when: "  ".into(),
                ..valid_scenario()
            }],
            ..valid_requirement()
        }],
        ..valid_specify()
    };
    assert_eq!(command.validate().unwrap_err().code(), "invalid_scenario");

    let command = SpecifyCommand {
        requirements: vec![RequirementDraft {
            scenarios: vec![ScenarioDraft {
                then: "\t".into(),
                ..valid_scenario()
            }],
            ..valid_requirement()
        }],
        ..valid_specify()
    };
    assert_eq!(command.validate().unwrap_err().code(), "invalid_scenario");

    let command = SpecifyCommand {
        requirements: vec![RequirementDraft {
            scenarios: vec![ScenarioDraft {
                given: None,
                ..valid_scenario()
            }],
            ..valid_requirement()
        }],
        ..valid_specify()
    };
    assert!(command.validate().is_ok());
}

#[test]
fn a_well_formed_specify_command_is_accepted() {
    assert!(valid_specify().validate().is_ok());
}

// --- PlanCommand -----------------------------------------------------------

/// A plan with no tasks is a plan that plans nothing.
#[test]
fn a_plan_must_carry_at_least_one_task() {
    let command = PlanCommand {
        tasks: vec![],
        ..valid_plan()
    };

    assert_eq!(command.validate().unwrap_err().code(), "missing_tasks");
}

/// Two tasks sharing a number in the same plan would collide in the store's
/// unique index; catching it here names exactly what was duplicated instead
/// of surfacing a raw `SQLite` error.
#[test]
fn task_numbers_must_not_repeat_within_a_plan() {
    let command = PlanCommand {
        tasks: vec![valid_task(), valid_task()],
        ..valid_plan()
    };

    assert_eq!(
        command.validate().unwrap_err().code(),
        "duplicate_task_number"
    );
}

#[test]
fn a_well_formed_plan_is_accepted() {
    assert!(valid_plan().validate().is_ok());
}

// --- TaskDraft ---------------------------------------------------------

/// A run binds to its task by number, so the number must be unambiguous:
/// digits separated by single dots, with no leading or trailing dot.
#[test]
fn a_task_number_must_be_dot_separated_digits() {
    for rejected in ["", ".1", "1.", "1..2", "1.a", "a", "1.2.", "-1"] {
        let command = PlanCommand {
            tasks: vec![TaskDraft {
                number: rejected.into(),
                ..valid_task()
            }],
            ..valid_plan()
        };
        assert_eq!(
            command.validate().unwrap_err().code(),
            "invalid_task_number",
            "{rejected} should not be a valid task number"
        );
    }

    for accepted in ["1", "1.2", "3.10.4"] {
        let command = PlanCommand {
            tasks: vec![TaskDraft {
                number: accepted.into(),
                ..valid_task()
            }],
            ..valid_plan()
        };
        assert!(
            command.validate().is_ok(),
            "{accepted} should be a valid task number"
        );
    }
}

/// A blank title, `verify_command` or `expected_output` is not a task,
/// whatever whitespace it is padded with.
#[test]
fn a_task_title_verify_command_and_expected_output_must_not_be_blank() {
    let command = PlanCommand {
        tasks: vec![TaskDraft {
            title: "   ".into(),
            ..valid_task()
        }],
        ..valid_plan()
    };
    assert_eq!(command.validate().unwrap_err().code(), "invalid_task_title");

    let command = PlanCommand {
        tasks: vec![TaskDraft {
            verify_command: "\t".into(),
            ..valid_task()
        }],
        ..valid_plan()
    };
    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_verify_command"
    );

    let command = PlanCommand {
        tasks: vec![TaskDraft {
            expected_output: vec!["\n".into()],
            ..valid_task()
        }],
        ..valid_plan()
    };
    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_expected_output"
    );
}

/// A task naming no marker at all is refused the same way a blank one is: a
/// command whose output nothing is checked against is a command nobody can
/// judge the result of.
#[test]
fn a_task_expected_output_must_name_at_least_one_marker() {
    let command = PlanCommand {
        tasks: vec![TaskDraft {
            expected_output: Vec::new(),
            ..valid_task()
        }],
        ..valid_plan()
    };
    assert_eq!(
        command.validate().unwrap_err().code(),
        "invalid_expected_output"
    );
}

/// A word that merely contains a placeholder is not one.
///
/// `todo` sits inside `mastodon`, and a plan for a to-do list would be refused
/// for naming its own subject. Rejecting valid work costs more than missing a
/// stub: the stub surfaces on the agent that hits it, while a false refusal
/// blocks now and offers no way around itself.
#[test]
fn a_word_that_merely_contains_a_placeholder_is_accepted() {
    for innocent in [
        "Import posts from Mastodon",
        "Render the todos list",
        "Fix TODOS.md parsing",
    ] {
        let command = PlanCommand {
            tasks: vec![TaskDraft {
                title: innocent.into(),
                ..valid_task()
            }],
            ..valid_plan()
        };

        assert!(
            command.validate().is_ok(),
            "{innocent:?} is ordinary prose, not a stub"
        );
    }
}

/// A plan with a stub in its text looks finished and falls apart on the
/// agent executing it, who sees only this one task and has no way to guess
/// what was meant. Cheaper to refuse at write time than to discover it mid
/// task. Checked without regard to case, and across every field a plan
/// writes prose into.
#[test]
fn a_task_must_not_contain_placeholder_text() {
    let placeholders = [
        "TBD",
        "TODO",
        "implement later",
        "fill in details",
        "add appropriate error handling",
        "handle edge cases",
        "similar to task",
    ];

    for placeholder in placeholders {
        // Mixed case, to prove the check is not case-sensitive.
        let mixed_case = placeholder
            .chars()
            .enumerate()
            .map(|(index, character)| {
                if index % 2 == 0 {
                    character.to_ascii_uppercase()
                } else {
                    character.to_ascii_lowercase()
                }
            })
            .collect::<String>();

        for field in ["title", "body", "verify_command", "expected_output"] {
            let task = match field {
                "title" => TaskDraft {
                    title: format!("Wire up retries ({mixed_case})"),
                    ..valid_task()
                },
                "body" => TaskDraft {
                    body: Some(format!("Steps: ...{mixed_case}...")),
                    ..valid_task()
                },
                "verify_command" => TaskDraft {
                    verify_command: format!("echo {mixed_case}"),
                    ..valid_task()
                },
                "expected_output" => TaskDraft {
                    expected_output: vec![format!("output ({mixed_case})")],
                    ..valid_task()
                },
                _ => unreachable!(),
            };

            let command = PlanCommand {
                tasks: vec![task],
                ..valid_plan()
            };
            assert_eq!(
                command.validate().unwrap_err().code(),
                "placeholder_text_in_task",
                "{field} containing {mixed_case:?} should be rejected"
            );
        }
    }
}

// --- ArchiveCommand ----------------------------------------------------

/// Everything an archive needs checked — is the change real, are all its
/// tasks done — is knowledge only the store has. There is no form-level rule
/// to enforce here, so `validate` always succeeds.
#[test]
fn an_archive_command_has_nothing_to_validate() {
    assert!(valid_archive().validate().is_ok());
}

// --- wire shape ------------------------------------------------------------

/// The string values must match the migration's `CHECK` constraints
/// verbatim (`crates/magent-store/migrations/0007_sdd.sql`): a wrong
/// `rename_all` here would compile clean and only surface later, as a
/// constraint violation in the store, far from where it was introduced.
#[test]
fn enums_use_their_snake_case_names() {
    let cases: Vec<(String, &str)> = vec![
        (
            serde_json::to_string(&Classification::Architectural).unwrap(),
            "\"architectural\"",
        ),
        (
            serde_json::to_string(&magent_core::ChangeStatus::Executing).unwrap(),
            "\"executing\"",
        ),
        (
            serde_json::to_string(&DeltaOp::Renamed).unwrap(),
            "\"renamed\"",
        ),
    ];

    for (actual, expected) in cases {
        assert_eq!(actual, expected);
    }
}

#[test]
fn change_ids_serialize_as_bare_strings() {
    let encoded = serde_json::to_string(&magent_core::ChangeId::new()).expect("serialize");
    assert!(encoded.starts_with('"'), "{encoded}");
    assert_eq!(encoded.trim_matches('"').len(), 36);
}

#[test]
fn a_propose_command_round_trips() {
    let command = valid_propose();
    let encoded = serde_json::to_string(&command).expect("serialize");
    let decoded: ProposeCommand = serde_json::from_str(&encoded).expect("deserialize");

    assert_eq!(decoded, command);
}

#[test]
fn a_specify_command_round_trips() {
    let command = valid_specify();
    let encoded = serde_json::to_string(&command).expect("serialize");
    let decoded: SpecifyCommand = serde_json::from_str(&encoded).expect("deserialize");

    assert_eq!(decoded, command);
}

#[test]
fn a_plan_command_round_trips() {
    let command = valid_plan();
    let encoded = serde_json::to_string(&command).expect("serialize");
    let decoded: PlanCommand = serde_json::from_str(&encoded).expect("deserialize");

    assert_eq!(decoded, command);
}

#[test]
fn an_archive_command_round_trips() {
    let command = valid_archive();
    let encoded = serde_json::to_string(&command).expect("serialize");
    let decoded: ArchiveCommand = serde_json::from_str(&encoded).expect("deserialize");

    assert_eq!(decoded, command);
}
