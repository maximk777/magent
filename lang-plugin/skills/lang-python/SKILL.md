---
name: lang-python
description: Use when working in a Python repository — before installing, testing or linting, and before claiming a change is verified. Determines the environment manager from the lockfile, and which interpreter commands actually run in.
---

# Python, in this repository

## Which environment

Read this before running anything, because the wrong interpreter fails in ways
that look like the code is broken:

| Marker | Manager | Run with |
|---|---|---|
| `uv.lock` | uv | `uv run <command>` |
| `poetry.lock` | poetry | `poetry run <command>` |
| `Pipfile.lock` | pipenv | `pipenv run <command>` |
| `.venv/` and none of the above | plain venv | `.venv/bin/python`, `.venv/bin/pytest` |
| `requirements.txt` only | pip into some environment | ask before installing anything |

A bare `pytest` runs whatever is first on PATH, which is usually not the
project's environment. Imports then fail for a missing package that is in fact
installed — in the environment you did not use.

## Read next

- `pyproject.toml` — `[project]` for the Python version, `[tool.ruff]`,
  `[tool.mypy]`, `[tool.pytest.ini_options]` for what the repository configured.
- `setup.cfg` / `tox.ini` — older homes for the same settings.
- Which linter: ruff, flake8, black, mypy, pyright. Their presence in config is
  the repository's choice; do not substitute one for another.

Ask `magent_search` for what is already known about this repository first.

## The commands

| Intent | Command |
|---|---|
| Tests | `<runner> pytest` |
| Types | `<runner> mypy .` or `<runner> pyright`, whichever is configured |
| Lint | `<runner> ruff check .` |
| Format | `<runner> ruff format --check .` — `--check` reports; without it, it rewrites |

## Before saying it is done

Run the checks. Report failures with their output. Say which interpreter you
used if it was not the obvious one.

## Worth remembering

Record the environment manager, the interpreter path and the real test command
with `magent_remember`, citing the lockfile or `pyproject.toml`.
