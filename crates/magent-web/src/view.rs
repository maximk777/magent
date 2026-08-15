//! What a page shows, already in words.
//!
//! The templates receive strings rather than domain values. Askama resolves a
//! custom filter as a generated type, which ties the templates to a macro
//! contract that changes between versions; turning a `FactScope` into the word
//! "repository" here owes that nothing and puts the wording next to the reason
//! for it.

use magent_core::{Cardinality, Fact, FactKind, FactScope, FactStatus};

/// One fact, as a page shows it.
pub struct FactView {
    pub id: String,
    pub name: String,
    pub title: String,
    pub body: String,
    pub namespace: String,
    pub scope: &'static str,
    pub status: &'static str,
    pub kind: &'static str,
    pub cardinality: &'static str,
    pub trust: &'static str,
    pub updated: String,
    pub evidence: Vec<EvidenceView>,
    pub revoked: bool,
}

pub struct EvidenceView {
    pub locator: String,
    pub excerpt: String,
}

impl FactView {
    #[must_use]
    pub fn of(fact: &Fact) -> Self {
        Self {
            id: fact.fact_id.to_string(),
            name: fact.name.clone(),
            title: fact.title.clone(),
            body: fact.body.clone(),
            namespace: fact.namespace.clone().unwrap_or_else(|| "—".to_owned()),
            scope: scope(fact.scope),
            status: status(fact.status),
            kind: kind(fact.kind),
            cardinality: cardinality(fact.cardinality),
            trust: trust(fact.confidence),
            updated: when(fact.updated_at),
            revoked: fact.status == FactStatus::Revoked,
            evidence: fact
                .evidence
                .iter()
                .map(|item| EvidenceView {
                    locator: item.locator.clone(),
                    excerpt: item.excerpt.clone().unwrap_or_default(),
                })
                .collect(),
        }
    }
}

#[must_use]
pub fn scope(value: FactScope) -> &'static str {
    match value {
        FactScope::User => "user",
        FactScope::Workspace => "workspace",
        FactScope::Repository => "repository",
        FactScope::Run => "run",
    }
}

#[must_use]
pub fn status(value: FactStatus) -> &'static str {
    match value {
        FactStatus::Observed => "observed",
        FactStatus::Inferred => "inferred",
        FactStatus::Verified => "verified",
        FactStatus::Contradicted => "contradicted",
        FactStatus::Stale => "stale",
        FactStatus::Revoked => "revoked",
    }
}

#[must_use]
pub fn kind(value: FactKind) -> &'static str {
    match value {
        FactKind::User => "about the user",
        FactKind::Feedback => "how to work",
        FactKind::Project => "about the project",
        FactKind::Reference => "a pointer",
    }
}

/// Says what a cardinality means rather than naming it.
///
/// "single" tells a reader nothing; "one current value" tells them what happens
/// when they write another.
#[must_use]
pub fn cardinality(value: Cardinality) -> &'static str {
    match value {
        Cardinality::Single => "one current value",
        Cardinality::Set => "several coexist",
        Cardinality::Timeline => "true over an interval",
    }
}

/// Confidence as words. A number between zero and one invites false precision;
/// these three bands are what anyone acts on anyway.
#[must_use]
pub fn trust(value: f64) -> &'static str {
    if value >= 0.9 {
        "high"
    } else if value >= 0.65 {
        "ordinary"
    } else {
        "low"
    }
}

/// A timestamp as elapsed time: the only question anyone asks of these.
#[must_use]
pub fn when(value: chrono::DateTime<chrono::Utc>) -> String {
    let minutes = (chrono::Utc::now() - value).num_minutes().max(0);

    match minutes {
        0 => "just now".to_owned(),
        1..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}
