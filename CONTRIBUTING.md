# Contributing to `decon`

Thanks for helping make `decon` a fast, reliable tool for turning code monoliths
into structured tutorials. This guide covers how to build, test, and land
changes in the workspace.

## Quick start

You need a recent Rust toolchain. The workspace declares `rust-version = "1.85"`.

```bash
# Clone and enter the workspace
git clone https://github.com/igmarin/decon-rs.git
cd decon-rs

# Build the whole workspace
cargo build --workspace

# Run the CLI
cargo run -p decon-cli -- --help
cargo run -p decon-cli -- crawl --dir tests/fixtures/python-lib
cargo run -p decon-cli -- dry-run --dir tests/fixtures/umbrella --format json
cargo run -p decon-cli -- eval --out tests/fixtures/tutorials/good-mini

# Full generate pipeline (requires DEEPSEEK_API_KEY or DECON_LLM_API_KEY)
cargo run -p decon-cli -- generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorial --language en
```

## Development workflow

We use a test-driven workflow for every behavior change, bug fix, or new
helper.

1. **Write a failing test** that expresses the contract or reproduces the bug.
2. **Run the test** and confirm it fails for the *right* reason.
3. **Implement the smallest change** that makes the test pass.
4. **Run the test again** and confirm it passes.
5. **Refactor** with the test suite still green.
6. Add edge cases or property tests only after the happy path is locked.

For library code this is non-negotiable. For CLI-only plumbing, still add an
integration test where the contract is observable (argument parsing, exit codes,
JSON dry-run shape, etc.).

## Running checks

The CI pipeline runs the following on every PR. Run them locally before pushing:

```bash
# Formatting
cargo fmt --all -- --check

# Clippy with warnings-as-errors
cargo clippy --workspace --all-targets -- -D warnings

# Tests
cargo test --workspace

# Docs (also enforces missing rustdoc on public items)
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Security audit
cargo audit

# Supply-chain policy (advisories, licenses, bans)
cargo deny check

# Fixture baseline check (verify baseline.json matches tests/fixtures/)
rustc tests/fixtures/regenerate_baseline.rs -o /tmp/regen_baseline && \
  /tmp/regen_baseline tests/fixtures/ --check

# Eval regression gate (good-mini passes at threshold 80)
cargo run -p decon-cli -- eval --out tests/fixtures/tutorials/good-mini --threshold 80
```


## Optional live LLM tests

Live tests make **real, paid API calls** (to DeepSeek by default) and cost a
few cents per run. They are feature-gated behind `decon-pipeline/live-llm`
and skip automatically (printing `skipped:` to stderr) when no API key is
present, so enabling the feature is always safe.

Default CI does **not** enable `live-llm`, so these tests are never compiled
or run there.

```sh
# Run live smoke tests (requires DEEPSEEK_API_KEY or DECON_LLM_API_KEY)
cargo test --workspace --features decon-pipeline/live-llm \
  --test live_smoke -- --nocapture
```

Budget is capped at `DECON_MAX_LLM_CALLS` (default `5`) calls per test via a
`ProgressTracker`. The identify test also writes responses to a disk cache
under `target/decon-llm-cache` so re-runs with an unchanged prompt are free.


## Pre-commit review (rs-guard)

Before committing non-trivial changes on a feature branch:

```bash
git add -A   # stage the change set you intend to commit
rs-guard --prompt-file .github/review-prompt.md
```

Address Critical / Security / Important findings (or document why not), then
commit. PRs also receive an automated rs-guard review from GitHub Actions.


## Domain modules (M1–M4)

| Area | Crate / path | Notes |
|------|----------------|-------|
| Module keys / inventory | `decon-core::module` | Pure |
| Scope filter (`--apps`) | `decon-core::scope` | Pure |
| Setup assessment | `decon-core::setup` | Pure |
| Context budget | `decon-core::budget` | Pure |
| Mermaid sanitize | `decon-core::mermaid` | Pure; table-driven tests |
| Index diagrams | `decon-core::diagrams` | Always sanitize/validate |
| Structural eval | `decon-core::eval` | Fixtures under `tests/fixtures/tutorials/` |
| RunConfig | `decon-core::config` | CLI > file > env > defaults |
| Checkpoint types | `decon-core::checkpoint` | ADR 0001 metadata; ADR 0006 stage outputs |
| Progress / LLM budget | `decon-core::progress` | Fail-closed max calls |
| Secrets redaction | `decon-core::secrets` | Paths + content heuristics |
| i18n chrome | `decon-core::i18n` | `Locale` + `ChromeStrings` (en/es); ADR 0007 |
| Chapter domain types | `decon-core::chapter` | `Chapter`, `ChapterOrder`, `ChapterResult` |
| M4 domain types | `decon-core::generate` | `SetupGuide`, `ArchitectureOverview`, `CombinedTutorial` |
| M4 domain types | `decon-core::abstraction` | `RelationshipsResult`, `Relationship` |
| LLM disk cache | `decon-llm::cache` | Hash-keyed response cache |
| LLM provider client | `decon-llm::openai_client` | OpenAI-compatible HTTP + retry/backoff |
| Bounded concurrency | `decon-llm::concurrency` | Semaphore-gated map batches |
| Checkpoint store | `decon-pipeline::checkpoint_store` | save/load bundle; file-based stage outputs (ADR 0006) |
| Resume helpers | `decon-pipeline::resume` | stage-skip / invalidate |
| Local crawl | `decon-crawl::local` | FS I/O |
| Dry-run plan | `decon-pipeline::dry_run` | Orchestration |
| Identify stage | `decon-pipeline::identify` | Map/reduce + single-shot |
| Relationships stage | `decon-pipeline::relationships` | Budgeted evidence selection |
| Order stage | `decon-pipeline::order` | Chapter ordering + validation |
| Chapters stage | `decon-pipeline::chapters` | Bounded-concurrent chapter writing |
| Setup guide stage | `decon-pipeline::setup_guide` | Score-triggered generation |
| Overview stage | `decon-pipeline::overview` | Multi-app architecture overview |
| Combine stage | `decon-pipeline::combine` | Index + diagrams + i18n chrome + sanitize |
| Generate orchestration | `decon-pipeline::generate` | Full pipeline + `--each-app` fan-out |
| Chapter review | `decon-pipeline::review` | `--review-chapters` second LLM pass |
| Prompt rendering | `decon-pipeline::prompts` | minijinja templates |
| CLI | `decon-cli` | Thin wrappers + `assert_cmd` tests |

## Coverage gate

CI enforces **≥ 85% workspace line coverage** via
`cargo llvm-cov --workspace --fail-under-lines 85` (M2). See
[`docs/m2-coverage-baseline.md`](docs/m2-coverage-baseline.md).

Locally:

```bash
cargo llvm-cov --workspace --fail-under-lines 85
```

We use
`cargo-llvm-cov`:

```bash
# Install cargo-llvm-cov (once)
cargo install cargo-llvm-cov

# Generate and view a summary
cargo llvm-cov --workspace --lcov --output-path target/lcov.info
cargo llvm-cov report --summary-only
```

The hard coverage gate (≥85% workspace lines) has been active since
Milestone 2 and runs on every PR via `cargo llvm-cov --workspace
--fail-under-lines 85`.

## Code conventions

- **Library crates** (`decon-core`, `decon-crawl`, `decon-llm`,
  `decon-pipeline`) perform no CLI or main-binary logic and stay easy to unit
  test.
- **Public APIs must have rustdoc.** Each library crate declares
  `#![deny(missing_docs)]`.
- Prefer typed errors with `thiserror` in libraries; user-facing messages and
  exit codes live in `decon-cli`.
- Use `clap` derive for CLI flags. Keep the binary a thin wrapper around the
  pipeline crates.
- All GitHub Actions we use are pinned by full commit SHA.

## Crate layout

```text
crates/
  decon-core/      pure domain: models, traits, mermaid, budgeting
  decon-crawl/     local + GitHub fetching
  decon-llm/       provider clients, retries, caching
  decon-pipeline/  stage orchestration, checkpoint/resume
  decon-cli/       clap binary and exit codes
```

Add new crates under `crates/` and register them in the root `Cargo.toml`
workspace `members` list.

## Documentation

- User-facing docs live in `README.md`, `docs/best-practices.md`, and
  `docs/move-to-rust.md`.
- Architecture decisions are recorded in `docs/adr/` when they affect the
  checkpoint format, crate boundaries, or provider contract.
- Every public function, type, and module should have a rustdoc comment.

## Pull requests

1. Create a feature branch from `main`: `feature/#-short-name` (e.g.
   `feature/3-contributing-guide`).
2. Make focused, incremental commits.
3. Ensure `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`,
   `cargo llvm-cov --fail-under-lines 85`, `cargo audit`, `cargo deny check`,
   and the eval regression gate (`cargo run -p decon-cli -- eval --out
   tests/fixtures/tutorials/good-mini --threshold 80`) pass locally.
4. Open a PR that references the issue number (e.g. `Closes #3`).
5. Wait for CI and any automated `rs-guard` review.
6. Merge only when CI is green.

## Questions?

Open an issue or discussion on [GitHub](https://github.com/igmarin/decon-rs).
