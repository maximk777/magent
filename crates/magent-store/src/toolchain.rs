//! Learning a repository's toolchain from what it declares.
//!
//! Every fact produced here says what a file contains, cites that file, and
//! stops. None of it claims a command was run, a linter exists on `PATH`, or a
//! version matches what CI uses — those need checking, and memory that asserts
//! them is worse than memory that stays quiet, because the agent will act on it
//! with confidence.
//!
//! Parsing is deliberately shallow. A repository can hold anything, including a
//! truncated or binary manifest, and detection runs at session start: it must
//! not be a way to break a session.

use std::{fmt::Write as _, path::Path};

use magent_core::{
    Cardinality, Evidence, FactKind, FactScope, FactStatus, OperationId, RememberCommand,
};

/// Read out of a file rather than checked by running it.
const DETECTED_CONFIDENCE: f64 = 0.9;

/// Largest manifest worth reading.
///
/// A lockfile can be megabytes and holds nothing this needs; the cap keeps a
/// pathological repository from stalling session start.
const MAX_MANIFEST_BYTES: u64 = 512 * 1024;

/// Facts a repository's manifests support.
///
/// Empty for a directory that declares nothing, which is a normal state rather
/// than a failure.
#[must_use]
pub fn detect_toolchain(root: &Path) -> Vec<RememberCommand> {
    let mut facts = Vec::new();

    detect_go(root, &mut facts);
    detect_rust(root, &mut facts);
    detect_node(root, &mut facts);
    detect_python(root, &mut facts);
    detect_jvm(root, &mut facts);
    detect_make(root, &mut facts);

    facts
}

// --- Go --------------------------------------------------------------------

fn detect_go(root: &Path, facts: &mut Vec<RememberCommand>) {
    let Some(manifest) = read(root, "go.mod") else {
        return;
    };

    let module = directive(&manifest, "module");
    let version = directive(&manifest, "go");

    let title = match &version {
        Some(version) => format!("Go {version}"),
        None => "Go module".to_owned(),
    };

    let mut body = String::from("Declared in go.mod");
    if let Some(module) = &module {
        let _ = write!(body, "; module {module}");
    }
    body.push_str(
        ".\n\nThe conventional commands are `go test ./...` and `go build ./...`. \
         The Go version above is what the module declares, which is not necessarily \
         what CI installs.",
    );

    facts.push(fact("toolchain-go", &title, &body, vec![cite("go.mod")]));

    for config in [".golangci.yml", ".golangci.yaml", ".golangci.toml"] {
        if root.join(config).is_file() {
            facts.push(fact(
                "linter-go",
                "golangci-lint is configured",
                "A configuration file is present, so the project intends to use \
                 golangci-lint. Whether the binary exists, and at which version, \
                 has not been checked here.",
                vec![cite(config)],
            ));
            break;
        }
    }
}

// --- Rust ------------------------------------------------------------------

fn detect_rust(root: &Path, facts: &mut Vec<RememberCommand>) {
    if read(root, "Cargo.toml").is_none() {
        return;
    }

    let mut evidence = vec![cite("Cargo.toml")];

    // The pinned channel lives in rust-toolchain.toml, not Cargo.toml. Reporting
    // one as the other is a mistake that has cost real time before.
    let pinned = read(root, "rust-toolchain.toml").and_then(|text| quoted_value(&text, "channel"));
    if pinned.is_some() {
        evidence.push(cite("rust-toolchain.toml"));
    }

    let title = match &pinned {
        Some(channel) => format!("Rust pinned to {channel}"),
        None => "Rust, no pinned toolchain".to_owned(),
    };

    let body = match &pinned {
        Some(_) => "The channel comes from rust-toolchain.toml. Conventional \
             commands are `cargo test`, `cargo clippy --all-targets` and \
             `cargo fmt --all --check`."
            .to_owned(),
        None => "No rust-toolchain.toml, so the version is whatever rustup \
             resolves for the caller. Conventional commands are `cargo test`, \
             `cargo clippy --all-targets` and `cargo fmt --all --check`."
            .to_owned(),
    };

    facts.push(fact("toolchain-rust", &title, &body, evidence));
}

// --- Node ------------------------------------------------------------------

fn detect_node(root: &Path, facts: &mut Vec<RememberCommand>) {
    let Some(manifest) = read(root, "package.json") else {
        return;
    };

    let parsed: Option<serde_json::Value> = serde_json::from_str(&manifest).ok();
    let mut evidence = vec![cite("package.json")];

    // The lockfile is the only honest signal of which package manager is in use:
    // package.json rarely says, and guessing npm is how a pnpm workspace gets
    // broken.
    let manager = [
        ("bun.lockb", "bun"),
        ("bun.lock", "bun"),
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("package-lock.json", "npm"),
    ]
    .into_iter()
    .find(|(lockfile, _)| root.join(lockfile).is_file());

    if let Some((lockfile, _)) = manager {
        evidence.push(cite(lockfile));
    }

    let body = match manager {
        Some((lockfile, name)) => {
            format!("Package manager is {name}, from the presence of {lockfile}.")
        }
        None => "No lockfile, so the package manager is unknown. Do not assume npm.".to_owned(),
    };

    facts.push(fact("toolchain-node", "Node.js project", &body, evidence));

    let scripts: Vec<(String, String)> = parsed
        .as_ref()
        .and_then(|value| value.get("scripts"))
        .and_then(serde_json::Value::as_object)
        .map(|scripts| {
            scripts
                .iter()
                .filter_map(|(name, command)| {
                    command
                        .as_str()
                        .map(|command| (name.clone(), command.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();

    if !scripts.is_empty() {
        let mut body = String::from("Declared in package.json:\n");
        for (name, command) in &scripts {
            let _ = writeln!(body, "- `{name}`: `{command}`");
        }
        body.push_str("\nThese are what the project declares; none has been run here.");

        facts.push(fact(
            "commands-node",
            "Scripts this project declares",
            &body,
            vec![cite("package.json")],
        ));
    }
}

// --- Python ----------------------------------------------------------------

fn detect_python(root: &Path, facts: &mut Vec<RememberCommand>) {
    let Some(manifest) = read(root, "pyproject.toml") else {
        return;
    };

    let requires = quoted_value(&manifest, "requires-python");
    let title = match &requires {
        Some(requires) => format!("Python {requires}"),
        None => "Python project".to_owned(),
    };

    let mut body = String::from("Declared in pyproject.toml.");
    for (marker, note) in [
        ("[tool.poetry]", "Managed with Poetry."),
        ("[tool.uv]", "Managed with uv."),
        ("[tool.hatch", "Managed with Hatch."),
        ("[tool.ruff", "Ruff is configured."),
    ] {
        if manifest.contains(marker) {
            body.push(' ');
            body.push_str(note);
        }
    }

    facts.push(fact(
        "toolchain-python",
        &title,
        &body,
        vec![cite("pyproject.toml")],
    ));
}

// --- JVM -------------------------------------------------------------------

fn detect_jvm(root: &Path, facts: &mut Vec<RememberCommand>) {
    let gradle = ["build.gradle", "build.gradle.kts"]
        .into_iter()
        .find(|manifest| root.join(manifest).is_file());
    let maven = root.join("pom.xml").is_file().then_some("pom.xml");

    let Some(manifest) = gradle.or(maven) else {
        return;
    };

    let mut evidence = vec![cite(manifest)];
    let mut body = String::new();

    if gradle.is_some() {
        // The wrapper pins the build tool's own version, so invoking a system
        // gradle is a different build from the one CI runs.
        if root.join("gradlew").is_file() {
            evidence.push(cite("gradlew"));
            body.push_str(
                "Gradle, with a wrapper: use `./gradlew` rather than a system gradle, \
                 since the wrapper pins the build's own version.",
            );
        } else {
            body.push_str("Gradle, with no wrapper checked in.");
        }
    } else {
        body.push_str("Maven. Conventional commands are `mvn test` and `mvn package`.");
    }

    body.push_str(
        "\n\nThe JDK version has not been checked here; it is set by the environment \
         or by CI rather than by this file alone.",
    );

    facts.push(fact("toolchain-jvm", "JVM project", &body, evidence));
}

// --- Make ------------------------------------------------------------------

fn detect_make(root: &Path, facts: &mut Vec<RememberCommand>) {
    let Some(makefile) = read(root, "Makefile") else {
        return;
    };

    let targets: Vec<String> = makefile
        .lines()
        .filter(|line| !line.starts_with('\t') && !line.starts_with('#'))
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let name = name.trim();
            // Skip variable assignments and pattern rules.
            if name.is_empty()
                || name.starts_with('.')
                || name.contains(['=', '%', '$', ' '])
                || rest.starts_with('=')
            {
                return None;
            }
            Some(name.to_owned())
        })
        .collect();

    if targets.is_empty() {
        return;
    }

    let body = format!(
        "Targets declared in the Makefile: {}.\n\nRun with `make <target>`. \
         None has been run here.",
        targets.join(", ")
    );

    facts.push(fact(
        "commands-make",
        "Make targets this project declares",
        &body,
        vec![cite("Makefile")],
    ));
}

// --- helpers ---------------------------------------------------------------

fn fact(name: &str, title: &str, body: &str, evidence: Vec<Evidence>) -> RememberCommand {
    RememberCommand {
        operation_id: OperationId::new(),
        name: name.to_owned(),
        title: title.to_owned(),
        body: body.to_owned(),
        kind: FactKind::Project,
        scope: FactScope::Repository,
        // Re-derived whenever a manifest changes, so a new value replaces the
        // old rather than piling up beside it.
        cardinality: Cardinality::Single,
        // Read, not run. Marking this verified would make the strongest status
        // the cheapest one to earn.
        status: FactStatus::Observed,
        confidence: DETECTED_CONFIDENCE,
        evidence,
        relates_to: vec![],
    }
}

/// Cites a path relative to the repository root, so the fact still resolves in
/// a different checkout of the same project.
fn cite(relative: &str) -> Evidence {
    Evidence {
        locator: relative.to_owned(),
        excerpt: None,
    }
}

/// Reads a manifest, refusing anything implausibly large.
fn read(root: &Path, relative: &str) -> Option<String> {
    let path = root.join(relative);
    let metadata = std::fs::metadata(&path).ok()?;

    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return None;
    }

    // Lossy: a manifest with invalid UTF-8 is malformed, not a reason to fail.
    std::fs::read(&path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// `go.mod`-style `<directive> <value>` at the start of a line.
fn directive(text: &str, keyword: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(keyword)?;
        let value = rest.strip_prefix(' ')?.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

/// TOML-style `key = "value"`, without pulling in a parser for two lookups.
fn quoted_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(key)?.trim_start();
        let value = rest.strip_prefix('=')?.trim();
        let unquoted = value.trim_matches('"').trim();
        (!unquoted.is_empty() && unquoted != value.trim_matches('\'')).then(|| unquoted.to_owned())
    })
}
