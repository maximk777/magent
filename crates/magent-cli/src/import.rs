//! Bringing existing memory across.
//!
//! There is a real corpus: a hundred-odd markdown facts filed per project, plus
//! a Codex archive of session summaries. This importer is deliberately
//! forgiving — some files have no frontmatter, some link to facts nobody ever
//! wrote — because a strict importer that refuses the awkward tenth is an
//! importer nobody runs, and the awkward tenth holds real knowledge.
//!
//! Nothing is invented. What cannot be read is reported, not guessed at.

use std::path::{Path, PathBuf};

use magent_core::{
    Cardinality, FactKind, FactScope, FactStatus, OperationId, RelationKind, RememberCommand,
};
use magent_store::{FactContext, Store};

/// Directories that hold facts about the user rather than about a project.
const PERSONAL_NAMESPACES: [&str; 2] = ["claude", "research"];

/// Indexes, not knowledge: a table of contents pointing at the real facts.
const INDEX_FILES: [&str; 2] = ["MEMORY.md", "README.md"];

/// Imported memory is trusted less than what was learned here: it is old, and
/// nothing was re-checked during the import.
const IMPORT_CONFIDENCE: f64 = 0.6;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportReport {
    pub facts: usize,
    pub relations: usize,
    /// Files that could not be read, with the reason. Reported rather than
    /// dropped: a silent import is indistinguishable from one that did nothing.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Imports `~/memory`-style markdown, one directory per project.
///
/// # Errors
///
/// Returns an error only when the store is unusable. An unreadable file is
/// recorded in the report and the import continues.
pub fn import_memory_dir(store: &Store, root: &Path) -> anyhow::Result<ImportReport> {
    let mut report = ImportReport::default();

    let Ok(entries) = std::fs::read_dir(root) else {
        // A missing corpus is a normal state, not a failure.
        return Ok(report);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let namespace = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        import_namespace(store, &path, &namespace, &mut report);
    }

    Ok(report)
}

fn import_namespace(store: &Store, dir: &Path, namespace: &str, report: &mut ImportReport) {
    let personal = PERSONAL_NAMESPACES.contains(&namespace);

    let Ok(entries) = std::fs::read_dir(dir) else {
        report.skipped.push((
            dir.to_path_buf(),
            "the directory could not be read".to_owned(),
        ));
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        if path
            .file_name()
            .is_some_and(|name| INDEX_FILES.contains(&name.to_string_lossy().as_ref()))
        {
            continue;
        }

        match import_fact_file(store, &path, namespace, personal) {
            Ok(relations) => {
                report.facts += 1;
                report.relations += relations;
            }
            Err(error) => report.skipped.push((path, format!("{error:#}"))),
        }
    }
}

fn import_fact_file(
    store: &Store,
    path: &Path,
    namespace: &str,
    personal: bool,
) -> anyhow::Result<usize> {
    let text = std::fs::read_to_string(path)?;
    let (frontmatter, body) = split_frontmatter(&text);

    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();

    // The filename is the fallback name: it is what the wikilinks in the corpus
    // already point at.
    let name = slugify(&field(&frontmatter, "name").unwrap_or(stem));
    let title = best_title(
        &name,
        [
            field(&frontmatter, "description"),
            field(&frontmatter, "title"),
            first_heading(body),
            opening_sentence(body),
        ],
    );

    let kind = field(&frontmatter, "type")
        .as_deref()
        .and_then(parse_kind)
        .unwrap_or(if personal {
            FactKind::User
        } else {
            FactKind::Project
        });

    let relations: Vec<(String, RelationKind)> = wikilinks(body)
        .into_iter()
        .map(|target| (target, RelationKind::Related))
        .collect();
    let relation_count = relations.len();

    let command = RememberCommand {
        operation_id: import_operation_id(namespace, &name),
        name,
        title,
        body: body.trim().to_owned(),
        kind,
        scope: if personal {
            FactScope::User
        } else {
            FactScope::Repository
        },
        // Imported facts are a snapshot: nothing in the corpus says whether two
        // notes about one subject compete or coexist, and guessing "single"
        // would silently supersede half of them on import.
        cardinality: Cardinality::Set,
        status: FactStatus::Observed,
        confidence: IMPORT_CONFIDENCE,
        evidence: vec![],
        relates_to: relations,
    };

    store.remember(
        &command,
        &FactContext {
            namespace: (!personal).then(|| namespace.to_owned()),
            provenance: "imported".to_owned(),
            ..FactContext::default()
        },
    )?;

    Ok(relation_count)
}

/// Imports Codex rollout summaries as reference facts.
///
/// The plan called for importing these as checkpoints. A checkpoint needs a run
/// and a session, so bringing a hundred summaries across would have meant
/// fabricating a hundred runs nobody ever worked on. They are records of past
/// sessions, which is what a reference fact is for.
///
/// # Errors
///
/// Returns an error only when the store is unusable.
pub fn import_codex_rollouts(store: &Store, root: &Path) -> anyhow::Result<ImportReport> {
    let mut report = ImportReport::default();

    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(report);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }

        match import_rollout(store, &path) {
            Ok(()) => report.facts += 1,
            Err(error) => report.skipped.push((path, format!("{error:#}"))),
        }
    }

    Ok(report)
}

fn import_rollout(store: &Store, path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;

    // The header is loose `key: value` lines before the first heading.
    let cwd = text
        .lines()
        .take_while(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix("cwd:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no cwd header, so the summary cannot be scoped"))?;

    let namespace = Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("the cwd header names no directory"))?;

    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = slugify(&stem);

    let title = first_heading(&text).unwrap_or_else(|| name.clone());

    store.remember(
        &RememberCommand {
            operation_id: import_operation_id(&namespace, &name),
            name,
            title,
            body: text.trim().to_owned(),
            kind: FactKind::Reference,
            scope: FactScope::Repository,
            cardinality: Cardinality::Set,
            status: FactStatus::Observed,
            confidence: IMPORT_CONFIDENCE,
            evidence: vec![],
            relates_to: vec![],
        },
        &FactContext {
            namespace: Some(namespace),
            provenance: "imported".to_owned(),
            ..FactContext::default()
        },
    )?;

    Ok(())
}

// --- parsing ---------------------------------------------------------------

/// Splits YAML frontmatter from the body, tolerating its absence.
fn split_frontmatter(text: &str) -> (String, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (String::new(), text);
    };
    let Some(end) = rest.find("\n---") else {
        return (String::new(), text);
    };

    let body = rest[end..]
        .strip_prefix("\n---")
        .unwrap_or("")
        .trim_start_matches('\n');

    (rest[..end].to_owned(), body)
}

/// Reads one scalar out of the frontmatter.
///
/// Deliberately not a YAML parser: the corpus uses a handful of scalar keys,
/// some nested one level under `metadata`, and pulling in a parser to read
/// `type:` would be more machinery than the job needs.
fn field(frontmatter: &str, key: &str) -> Option<String> {
    frontmatter.lines().find_map(|line| {
        let trimmed = line.trim();
        let value = trimmed.strip_prefix(key)?.strip_prefix(':')?.trim();
        (!value.is_empty()).then(|| value.trim_matches('"').to_owned())
    })
}

/// Picks the most informative title available.
///
/// Some files in the corpus carry `title: <the-slug-again>` and put the real
/// heading in the body. Taking the frontmatter at face value there produces an
/// index full of filenames, which tells the model nothing it did not already
/// know from the name, so a candidate that merely repeats the name is skipped.
fn best_title(name: &str, candidates: impl IntoIterator<Item = Option<String>>) -> String {
    let mut fallback = None;

    for candidate in candidates.into_iter().flatten() {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }
        if slugify(trimmed) == name {
            fallback.get_or_insert_with(|| trimmed.to_owned());
            continue;
        }
        return trimmed.to_owned();
    }

    fallback.unwrap_or_else(|| name.to_owned())
}

fn parse_kind(raw: &str) -> Option<FactKind> {
    match raw {
        "user" => Some(FactKind::User),
        "feedback" => Some(FactKind::Feedback),
        "project" => Some(FactKind::Project),
        "reference" => Some(FactKind::Reference),
        _ => None,
    }
}

fn first_heading(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.trim().strip_prefix("# "))
        .map(|heading| heading.trim().to_owned())
}

/// The first prose line, trimmed to a title's length.
///
/// Last resort for files that carry neither a description nor a heading.
/// Markup lines are skipped: a fenced block or a list bullet as the title would
/// be worse than the slug it replaces.
fn opening_sentence(body: &str) -> Option<String> {
    const LIMIT: usize = 110;

    let line = body
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with("```")
                && !line.starts_with('-')
                && !line.starts_with('|')
                && !line.starts_with('>')
        })?
        .trim_matches('*')
        .trim();

    if line.is_empty() {
        return None;
    }

    // Prefer a sentence boundary, so the title reads as a statement rather than
    // a fragment cut mid-word.
    let sentence = line.split_inclusive(". ").next().unwrap_or(line).trim();
    let chosen = if sentence.chars().count() <= LIMIT {
        sentence
    } else {
        line
    };

    Some(
        chosen
            .chars()
            .take(LIMIT)
            .collect::<String>()
            .trim()
            .trim_end_matches('.')
            .to_owned(),
    )
}

fn wikilinks(body: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = body;

    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };

        let target = slugify(after[..end].trim());
        if !target.is_empty() && !found.contains(&target) {
            found.push(target);
        }
        rest = &after[end + 2..];
    }

    found
}

/// Coerces a heading or filename into a valid fact name.
fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut pending_hyphen = false;

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_hyphen && !slug.is_empty() {
                slug.push('-');
            }
            pending_hyphen = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }

    slug.chars().take(120).collect()
}

/// A stable operation id per imported file.
///
/// Re-importing is normal: the corpus keeps being written to while Magent is
/// adopted. Deriving the id from namespace and name makes the second import a
/// replay of the first rather than a duplicate.
fn import_operation_id(namespace: &str, name: &str) -> OperationId {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    "magent-import".hash(&mut hasher);
    namespace.hash(&mut hasher);
    name.hash(&mut hasher);
    let low = hasher.finish();

    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    namespace.hash(&mut hasher);
    let high = hasher.finish();

    OperationId::from_uuid(uuid::Uuid::from_u64_pair(high, low))
}
