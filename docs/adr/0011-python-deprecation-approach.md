# ADR 0011: Python Deprecation Approach (Migration Guide vs Wrapper)

## Status

Accepted

## Date

2026-07-25

## Context

The original `decon` was a Python/PocketFlow reference implementation. It has
been rewritten in Rust as a single, fast, distributable binary (M1–M5). As of
M5 (#191), the Rust CLI is the canonical entrypoint, and the Python
entrypoint needs to be formally deprecated.

The question is **how** to deprecate the Python entrypoint:

- **Option A — Wrapper**: Keep a Python `decon` package that shells out to the
  Rust binary. Existing `pip install decon` users get the Rust binary
  transparently.
- **Option B — Migration guide only**: Publish a migration guide and deprecate
  the Python package without a wrapper. Users switch to `brew install decon`
  or `cargo install decon-cli`.

### Constraints

- The Python code lives in a **separate repository** from the Rust rewrite.
  The Rust workspace (`decon-rs`) does not contain the Python source.
- The product value (pipeline stages, prompts, quality heuristics) has been
  faithfully ported to Rust and validated against a frozen baseline.
- The Python entrypoint will receive no new features.
- Users need a clear, low-friction path to switch.

## Decision

Adopt **Option B — migration guide only** (no wrapper).

The Python entrypoint is deprecated via a comprehensive migration guide at
[`docs/migrating-from-python.md`](../migrating-from-python.md). The guide
covers:

1. **Command mapping** — `python main.py` → `decon`, with a table of
   equivalent subcommands and flags.
2. **Environment variable changes** — `LLM_PROVIDER` / `LLM_API_KEY` →
   `DECON_LLM_BASE_URL` / `DECON_LLM_API_KEY` / `DEEPSEEK_API_KEY`, with
   precedence rules.
3. **Feature parity table** — what works in both, what is Rust-only, what is
   Python-only (nothing of significance).
4. **FAQ** — common migration questions (checkpoint format, config file,
   provider setup).

The Python package is marked as deprecated (in its own repository) with a
notice pointing to the migration guide. No wrapper script is maintained.

### Why not a wrapper (Option A)?

- The Python code is in a separate repository, so a wrapper would require
  cross-repository coordination: the Python package would need to download or
  bundle the Rust binary, adding packaging complexity (platform detection,
  binary extraction, PATH management).
- A wrapper creates an **illusion of compatibility** that breaks subtly: if
  the Rust CLI changes flags or behavior, the wrapper's translation layer must
  be updated in lockstep. This is ongoing maintenance for a deprecated path.
- The Rust CLI has **more features** than the Python original (checkpoint
  resume, `--each-app`, `--review-chapters`, i18n, completions, man page). A
  wrapper that only exposes the Python-era surface would hide these
  improvements.
- The migration is **one-time** per user. A wrapper optimizes for gradual
  migration, but the switch is straightforward (install binary, update env
  vars) and the guide makes it explicit.

## Alternatives Considered

### Option A — Python wrapper shelling out to Rust binary

- **Pros**: `pip install decon` continues to work; zero-friction transition for
  existing Python users.
- **Cons**: Cross-repository packaging complexity; ongoing maintenance for a
  deprecated path; hides Rust-only features; creates illusion of compatibility.
- **Rejected**: The complexity and maintenance cost outweigh the convenience.
  The migration guide is a one-time read; the wrapper is perpetual
  maintenance.

### Option C — Hard removal (no deprecation period)

- **Pros**: Cleanest — no ambiguity about which entrypoint is canonical.
- **Cons**: Existing Python users get no warning and no migration path. Their
  workflows break silently.
- **Rejected**: A deprecation period with a migration guide is the standard,
  user-respectful approach.

### Option D — Keep both maintained indefinitely

- **Pros**: No user disruption.
- **Cons**: Doubles the maintenance burden. The Python code has known
  fragility (Make env bugs, dependency soup, untyped `shared` dict). The Rust
  rewrite exists precisely to solve these.
- **Rejected**: Defeats the purpose of the rewrite.

## Consequences

- **Positive**: The Rust CLI is unambiguously the canonical entrypoint. No
  wrapper maintenance burden.
- **Positive**: The migration guide is a single, comprehensive document that
  users read once and then operate entirely in the Rust ecosystem.
- **Positive**: Rust-only features (checkpoint resume, `--each-app`,
  completions, man page) are not hidden behind a Python-era API surface.
- **Negative**: `pip install decon` users must actively switch — there is no
  automatic transition. The migration guide mitigates this but requires users
  to read it.
- **Negative**: Any automation scripts that call `python main.py` directly must
  be updated to call `decon` instead. The command mapping table in the guide
  makes this mechanical.

## Related Documents

- [`docs/migrating-from-python.md`](../migrating-from-python.md) — the
  migration guide.
- [`README.md`](../../README.md) — mentions Python deprecation and links to
  the guide.
- [`docs/move-to-rust.md`](../move-to-rust.md) — Phase 4 documents the
  deprecation as complete.
- Issue #191 — Python entrypoint deprecation with migration guide.
