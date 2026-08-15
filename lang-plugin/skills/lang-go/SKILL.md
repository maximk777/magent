---
name: lang-go
description: Use when working in a Go repository — before running tests, vet or lint, and before claiming a change is verified. Establishes this repository's actual check commands from what it declares rather than assuming the defaults.
---

# Go, in this repository

## Read first

- `go.mod` — the module path and the declared Go version. The version is what
  the module asks for, not necessarily what is installed.
- `go.work` — if present, the build spans several modules and commands run from
  the workspace root behave differently from inside one module.
- `Makefile` — when it has `test`, `lint` or `check` targets, those are the
  repository's own answer and they usually wrap flags you would otherwise miss.
- `.golangci.yml` / `.yaml` / `.toml` — a lint configuration. Its presence says
  the repository lints; it does not say `golangci-lint` is installed. Check.

Ask `magent_search` for what is already known about this repository's checks
before deriving them again.

## The commands, and the traps in them

| Intent | Command | Why |
|---|---|---|
| Tests | `go test ./...` | The `./...` is what makes it recursive |
| Vet | `go vet ./...` | Catches real mistakes the compiler allows |
| Build | `go build ./...` | Does **not** compile test files; `go vet` does |
| Lint | `golangci-lint run` | Only when the config exists and the binary is on PATH |
| Format | `gofmt -l .` | Lists what is unformatted; `gofmt -w` rewrites instead of reporting |

Build tags hide code from all of these. If the repository uses them
(`//go:build integration`), tests behind a tag did not run unless you passed
`-tags`.

`go test` caches results. A pass that returns instantly with `(cached)` did not
execute; use `-count=1` when the environment changed underneath it.

## Before saying it is done

Run the checks. Report the failures with their output rather than summarising
them. Say explicitly if a build tag or a short-mode guard meant part of the
suite did not run.

## Worth remembering

Record durable findings with `magent_remember`, citing the file that says so —
the Makefile target that is the real entry point, a package that needs a tag, a
linter the repository configured but does not have installed.
