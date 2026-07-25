# decon

> Deconstruct massive code monoliths into structured, beginner-friendly tutorials — powered by LLMs, built in Rust.

`decon` crawls a codebase (local tree or GitHub URL), identifies its core
abstractions, and produces a multi-chapter Markdown + Mermaid tutorial that
explains how the system works — including setup, architecture, and
inter-concept relationships. It is designed for monorepos and large codebases
where "read the source" is not a realistic onboarding path.

This is a **Rust rewrite** of a Python/PocketFlow reference implementation. The
product value lives in the pipeline stages, prompt catalog, and quality
heuristics — not the runtime. See [`docs/move-to-rust.md`](docs/move-to-rust.md)
for the full migration design.

---

## Current status

**Milestone 3 (LLM Identify) — complete.**
Live LLM provider clients, map-reduce identify, and checkpoint resume are
working. M4 (full generate pipeline) is next.

| Milestone | Goal | Status |
|-----------|------|--------|
| **M0** — Spec Freeze | Workspace layout, CI, CONTRIBUTING, ADR 0001, prompt catalog, test fixtures, parity baseline | ✅ Done |
| **M1** — Crawl + Dry-run + Eval | `decon crawl` / dry-run matching `baseline.json`; mermaid sanitize; setup-assessment parity; `decon eval` port | ✅ Done |
| **M2** — Checkpoint, Config & Coverage | Content-addressed checkpoint (ADR 0001); `decon.toml`; ≥85% coverage gate | ✅ Done |
| **M3** — LLM Identify | `LlmClient` trait + provider clients; map/reduce identify; checkpoint resume; Ctrl+C graceful shutdown | ✅ Done |
| **M4** — Full Generate | Relationships → order → chapters → setup → overview → combine; Spanish chrome; `--each-app` | 🔜 Next |
| **M5** — Product Polish | Installers, man page, shell completions, concurrency, error UX | Planned |

### What works today

- **Cargo workspace** with five crates: `decon-core`, `decon-crawl`,
  `decon-llm`, `decon-pipeline`, `decon-cli`.
- **CLI (M1 + M2):**
  - `decon crawl --dir PATH [--format text|json]` — local file inventory
  - `decon dry-run --dir PATH [--apps …] [--format text|json]` — crawl + scope +
    setup assessment + budget (zero LLM)
  - `decon eval --out PATH` — structural tutorial quality gate (zero LLM)
  - `decon init` — write starter `decon.toml`
  - `decon resume --checkpoint PATH` — report next/pending stages from a checkpoint
- **`decon-core` pure helpers:** module keys, monorepo scope, setup scoring,
  context budget, Mermaid sanitize/validate, index diagram builders, structural
  eval, `RunConfig` layering, checkpoint schema v1 types, progress budget,
  secrets redaction.
- **Checkpoint store + resume** (`decon-pipeline`): `checkpoint.json` +
  `files.ndjson.gz`, stage-skip / partial regenerate helpers.
- **LLM provider client** (`decon-llm`): `OpenAiCompatibleClient` with
  retry/backoff/timeout, host allowlist validation, disk cache
  (key = hash(prompt)+model+provider), and bounded-concurrency map batches.
- **CI coverage hard gate:** ≥85% workspace line coverage.
- **Parity fixtures:** `tests/fixtures/{python-lib,umbrella,js-lib}` + frozen
  `baseline.json`; tutorial goldens under `tests/fixtures/tutorials/`.
- **CI pipeline:** fmt, clippy (`-D warnings`), test, coverage report, doc,
  `cargo audit`, fixture baseline check, rs-guard PR review.
- **Prompt catalog** (`prompts/`) and **ADR 0001** checkpoint schema (used from M2+).

### Quick start (M1)

```bash
cargo build -p decon-cli

# Inventory a repo
cargo run -p decon-cli -- crawl --dir tests/fixtures/python-lib --format json

# Dry-run plan (optionally scope monorepo apps)
cargo run -p decon-cli -- dry-run --dir tests/fixtures/umbrella --apps apps/alpha

# Structural eval of a tutorial tree
cargo run -p decon-cli -- eval --out tests/fixtures/tutorials/good-mini

# Config + checkpoint status
cargo run -p decon-cli -- init --dir /tmp/decon-demo
# cargo run -p decon-cli -- resume --checkpoint PATH --format json
```

### What does not work yet

Full `generate` pipeline (relationships, chapter writing, combine/index
chrome). Those land in M4.

---

## Workspace layout

```
decon-rs/
├── crates/
│   ├── decon-core/       # Pure domain models, traits, budgeting, mermaid sanitize
│   ├── decon-crawl/      # Local + GitHub crawling (gitignore-aware)
│   ├── decon-llm/        # LlmClient trait, provider clients, caching, retries
│   ├── decon-pipeline/   # Stage orchestration, checkpoint/resume, dry-run
│   └── decon-cli/        # Thin binary — clap args → pipeline wiring
├── prompts/              # 10 versioned Jinja2 templates (identify, relationships, chapters, …)
├── tests/fixtures/       # Minimal repos + frozen baseline.json + Rust regenerator
├── docs/
│   ├── move-to-rust.md   # Migration design: pipeline model, domain objects, phase plan
│   ├── best-practices.md # Language-agnostic product rules (scope, budget, quality, mermaid)
│   └── adr/              # Architecture Decision Records
├── .github/workflows/    # CI (fmt/clippy/test/cov/doc/audit/baseline) + rs-guard review
└── CONTRIBUTING.md       # TDD workflow, coverage gate, check commands
```

---

## Quick start

You need a recent Rust toolchain (≥ 1.85).

```bash
git clone https://github.com/igmarin/decon-rs.git
cd decon-rs

# Build the workspace
cargo build --workspace

# Run the CLI
cargo run -p decon-cli -- --help
```

### Run the test suite

```bash
cargo test --workspace
```

### Verify the fixture baseline

```bash
rustc tests/fixtures/regenerate_baseline.rs -o /tmp/regen_baseline
/tmp/regen_baseline tests/fixtures/ --check
```

This confirms `tests/fixtures/baseline.json` matches the fixture directories.
Use `--write` to regenerate after an intentional fixture change.

---

## Pipeline overview

```
fetch + scope → identify (map/reduce) → relationships → order chapters
  → write chapters + diagrams → setup guide? → architecture overview?
  → combine index + sanitize → eval output
```

Every expensive stage is **idempotent and checkpointed** so a long monorepo
run can resume after failure. The full design — domain objects, stage
contracts, checkpoint schema, provider model — lives in
[`docs/move-to-rust.md`](docs/move-to-rust.md).

### Parity testing

The Rust implementation is validated against a frozen baseline originally
produced by the Python reference. A standalone Rust regenerator
(`tests/fixtures/regenerate_baseline.rs`) reproduces that baseline
byte-for-byte without needing Python. When `decon-crawl` is built in M1, it
is tested against the same frozen `baseline.json` — two independent
implementations must agree on the same oracle.

---

## Development

We follow a **test-driven** workflow: write a failing test → run it →
implement the smallest change → run again → refactor. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the full guide, coverage gate, and
the list of CI checks to run locally.

### CI checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo llvm-cov --workspace --fail-under-lines 85
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo audit
cargo deny check
rustc tests/fixtures/regenerate_baseline.rs -o /tmp/regen_baseline && \
  /tmp/regen_baseline tests/fixtures/ --check
```

---

## AI code review automation

Every pull request is reviewed automatically by
[rs-guard](https://github.com/nebulaideas/rs-guard) using the project-specific
prompt in [`.github/review-prompt.md`](.github/review-prompt.md):

- **CI**: [`.github/workflows/rs-guard-review.yml`](.github/workflows/rs-guard-review.yml)
  posts an APPROVE / REQUEST_CHANGES / COMMENT review on every non-draft PR.
  Requires a `DEEPSEEK_API_KEY` repository secret.
- **Local (optional)**: run `./scripts/install-hooks.sh` once to install a
  pre-commit hook that reviews staged changes before commit (bypass with
  `git commit --no-verify`). Requires `rs-guard` on `PATH`
  (`cargo install rs-guard`) and `DEEPSEEK_API_KEY` in your environment or
  in `~/.config/rs-guard/env`.

---

## Documentation

| Document | What it covers |
|----------|---------------|
| [`docs/move-to-rust.md`](docs/move-to-rust.md) | Migration design, pipeline model, domain objects, phase plan, engineering bar |
| [`docs/best-practices.md`](docs/best-practices.md) | Language-agnostic product rules: scope, budget, abstraction quality, mermaid, setup docs |
| [`docs/adr/0001-checkpoint-schema-v1.md`](docs/adr/0001-checkpoint-schema-v1.md) | Content-addressed checkpoint format for resume |
| [`docs/adr/0002-async-trait-llm-client.md`](docs/adr/0002-async-trait-llm-client.md) | Why `async-trait` for `LlmClient` (object-safety tradeoff) |
| [`docs/adr/0003-bounded-concurrency-semaphore.md`](docs/adr/0003-bounded-concurrency-semaphore.md) | Why `tokio::sync::Semaphore` for bounded map batches |
| [`docs/adr/0004-yaml-parser-migration.md`](docs/adr/0004-yaml-parser-migration.md) | Migration off unsound `serde_yml`/`libyml` (superseded by 0005) |
| [`docs/adr/0005-yaml-parser-serde-yaml-ng.md`](docs/adr/0005-yaml-parser-serde-yaml-ng.md) | Migration to `serde_yaml_ng` (maintained fork) |
| [`prompts/README.md`](prompts/README.md) | Prompt catalog: 10 templates, variable schema, integration notes |
| [`tests/fixtures/README.md`](tests/fixtures/README.md) | Fixture set, baseline regenerator, parity strategy |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | TDD workflow, coverage gate, CI checks, PR process |

---

## License

MIT
