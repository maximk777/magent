//! Writing memory back out as markdown.
//!
//! This is what keeps the store from being a one-way door. Everything that goes
//! in comes back out in the format it came from — readable and editable without
//! Magent — so adopting the store is not a bet on one binary staying alive.
//!
//! The output is a corpus, not a dump: one file per fact, one index per
//! project, exactly what the importer reads back.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use magent_core::{Fact, FactKind, FactScope};
use magent_store::{FactQuery, Store};

/// Where facts with no project land.
///
/// The same directory the personal corpus already uses, so the export
/// re-imports as user scope instead of becoming some project's memory.
const PERSONAL_DIR: &str = "claude";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExportReport {
    pub facts: usize,
    pub namespaces: usize,
    pub root: PathBuf,
}

/// Writes every current fact under `root`, one directory per namespace.
///
/// # Errors
///
/// Fails if the store cannot be read or a file cannot be written.
pub fn export_memory_dir(store: &Store, root: &Path) -> anyhow::Result<ExportReport> {
    // Everything currently held: superseded and revoked facts are already
    // filtered out by the store, which is what the corpus should reflect.
    let facts = store.search(&FactQuery {
        text: None,
        namespaces: store.known_namespaces()?,
        workspace_id: None,
        limit: usize::MAX,
    })?;

    let mut by_directory: BTreeMap<String, Vec<Fact>> = BTreeMap::new();
    for fact in facts {
        let directory = fact
            .namespace
            .clone()
            .filter(|_| fact.scope != FactScope::User)
            .unwrap_or_else(|| PERSONAL_DIR.to_owned());
        by_directory.entry(directory).or_default().push(fact);
    }

    let mut report = ExportReport {
        root: root.to_path_buf(),
        ..ExportReport::default()
    };

    for (directory, mut facts) in by_directory {
        facts.sort_by(|left, right| left.name.cmp(&right.name));

        let target = root.join(&directory);
        std::fs::create_dir_all(&target)?;

        for fact in &facts {
            let relations = store.relations_of_id(fact.fact_id)?;
            std::fs::write(
                target.join(format!("{}.md", fact.name)),
                render_fact(fact, &relations),
            )?;
            report.facts += 1;
        }

        std::fs::write(target.join("MEMORY.md"), render_index(&facts))?;
        report.namespaces += 1;
    }

    Ok(report)
}

/// Renders one fact in the corpus's own frontmatter format.
fn render_fact(fact: &Fact, relations: &[String]) -> String {
    let mut out = String::from("---\n");
    let _ = writeln!(out, "name: {}", fact.name);
    let _ = writeln!(out, "description: {}", single_line(&fact.title));
    let _ = writeln!(out, "metadata:");
    let _ = writeln!(out, "  node_type: memory");
    let _ = writeln!(out, "  type: {}", kind_name(fact.kind));
    out.push_str("---\n\n");

    out.push_str(fact.body.trim());
    out.push('\n');

    if !relations.is_empty() {
        let links: Vec<String> = relations
            .iter()
            .map(|target| format!("[[{target}]]"))
            .collect();
        let _ = writeln!(out, "\nRelated: {}.", links.join(", "));
    }

    if !fact.evidence.is_empty() {
        out.push_str("\nEvidence:\n");
        for evidence in &fact.evidence {
            let _ = writeln!(out, "- `{}`", evidence.locator);
        }
    }

    out
}

/// Renders the index the corpus is navigated through.
fn render_index(facts: &[Fact]) -> String {
    let mut out = String::new();

    for fact in facts {
        let _ = writeln!(
            out,
            "- [{}]({}.md) — {}",
            fact.name,
            fact.name,
            single_line(&fact.title)
        );
    }

    out
}

/// Frontmatter values are single-line scalars, so a title that wrapped would
/// silently truncate the file's own header on the next read.
fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn kind_name(kind: FactKind) -> &'static str {
    match kind {
        FactKind::User => "user",
        FactKind::Feedback => "feedback",
        FactKind::Project => "project",
        FactKind::Reference => "reference",
    }
}
