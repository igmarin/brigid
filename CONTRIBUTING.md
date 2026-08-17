# Contributing to `brigid`

Thanks for helping make `brigid` a fast, reliable tool for turning code monoliths
into structured tutorials. This guide covers how to build, test, and land
changes in the workspace.

## Quick start

You need a recent Rust toolchain. The workspace declares `rust-version = "1.92"`
(llm-kernel's MSRV; 1.97+ is fine).

```bash
# Clone and enter the workspace
git clone https://github.com/igmarin/brigid.git
cd brigid

# Build the whole workspace
cargo build --workspace

# Run the CLI
cargo run -p brigid-cli -- --help
cargo run -p brigid-cli -- crawl --dir tests/fixtures/python-lib
cargo run -p brigid-cli -- dry-run --dir tests/fixtures/umbrella --format json
cargo run -p brigid-cli -- eval --out tests/fixtures/tutorials/good-mini

# Full generate pipeline (requires DEEPSEEK_API_KEY or BRIGID_LLM_API_KEY)
cargo run -p brigid-cli -- generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorial --language en
```

### Windows

`brigid` builds and tests natively on Windows with the MSVC toolchain. Install
[Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
(the C++ workload) and [Rust](https://rustup.rs/) via `rustup-init.exe`, then
use the same `cargo` commands. CI runs the test and clippy jobs on
`windows-latest` to catch platform-specific regressions.

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
cargo run -p brigid-cli -- eval --out tests/fixtures/tutorials/good-mini --threshold 80

# Benchmarks (criterion — optional, for performance tracking)
cargo bench -p brigid-pipeline
```


## Optional live LLM tests

Live tests make **real, paid API calls** (to DeepSeek by default) and cost a
few cents per run. They are feature-gated behind `brigid-pipeline/live-llm`
and skip automatically (printing `skipped:` to stderr) when no API key is
present, so enabling the feature is always safe.

Default CI does **not** enable `live-llm`, so these tests are never compiled
or run there.

```sh
# Run live smoke tests (requires DEEPSEEK_API_KEY or BRIGID_LLM_API_KEY)
cargo test --workspace --features brigid-pipeline/live-llm \
  --test live_smoke -- --nocapture
```

Budget is capped at `BRIGID_MAX_LLM_CALLS` (default `5`) calls per test via a
`ProgressTracker`. The identify test also writes responses to a disk cache
under `target/brigid-llm-cache` so re-runs with an unchanged prompt are free.


## Testing strategy

`brigid` uses several layers of tests, each suited to a different concern:

| Layer | Tooling | What it covers |
|-------|---------|----------------|
| **Unit tests** (`#[test]`) | `cargo test` | Pure domain logic in `brigid-core` (budget, scope, mermaid, eval, config, plugin) and stage helpers. Fast, no I/O. |
| **Integration tests** (`tests/`) | `cargo test --test …` | Stage orchestration, checkpoint roundtrips, CLI exit codes, JSON schema stability. |
| **HTTP contract tests** | [`wiremock`](https://crates.io/crates/wiremock) | LLM provider client retry/backoff, timeout, and error handling against a mock OpenAI-compatible server — no real network. |
| **Property tests** | [`proptest`](https://crates.io/crates/proptest) | Pure-logic invariants: budget packing, module-key normalization, mermaid sanitize idempotence. |
| **CLI tests** | [`assert_cmd`](https://crates.io/crates/assert_cmd) | `brigid --help`, exit codes, `--format json` dry-run shape, error messages. |
| **Live LLM smoke** | feature-gated `brigid-pipeline/live-llm` | Real, paid API calls (DeepSeek) against a tiny fixture; budget-capped via `BRIGID_MAX_LLM_CALLS`. Runs **nightly** in CI, never on PR/push. Skips automatically when no key is present. |
| **Eval regression** | `brigid eval` on golden fixtures | Structural tutorial quality gate (`good-mini` passes, `broken-mini` fails at threshold 80). |

Guidelines:

- **Prefer unit tests** for pure logic; keep `brigid-core` I/O-free so it stays
  fast and deterministic.
- Use **wiremock** for any HTTP-touching code — never hit a live provider in
  unit or PR CI.
- Use **proptest** for pure functions with many input combinations
  (budgeting, parsing, normalization).
- Use **assert_cmd** for observable CLI contracts (argument parsing, exit
  codes, stdout JSON shape).
- Live LLM tests are **opt-in and nightly only**; see
  [Optional live LLM tests](#optional-live-llm-tests).


## Pre-commit review (rs-guard)

Before committing non-trivial changes on a feature branch:

```bash
git add -A   # stage the change set you intend to commit
rs-guard --prompt-file .github/review-prompt.md
```

Address Critical / Security / Important findings (or document why not), then
commit. PRs also receive an automated rs-guard review from GitHub Actions.

### CI automation

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

A **release workflow** (`.github/workflows/release.yml`) triggers on tag push
(`vX.Y.Z`), builds a Linux x86_64 release binary, packages it with the man
page and completion scripts, generates a SHA-256 checksum, and creates a
GitHub Release with notes extracted from [`CHANGELOG.md`](CHANGELOG.md).
macOS and Windows users install from source through Homebrew or `cargo
install`. The workflow can also be dispatched manually in dry-run mode to
validate the build without publishing.

CI also runs a **nightly LLM smoke** job (scheduled, not on PR/push) that
generates a tutorial with a live DeepSeek key, evals the output, and compares
the score against the frozen `llm-generated` fixture — opening a GitHub issue
on regression.


## Domain modules (M1–M5)

| Area | Crate / path | Notes |
|------|----------------|-------|
| Module keys / inventory | `brigid-core::module` | Pure |
| Scope filter (`--apps`) | `brigid-core::scope` | Pure |
| Setup assessment | `brigid-core::setup` | Pure |
| Context budget | `brigid-core::budget` | Pure |
| Mermaid sanitize | `brigid-core::mermaid` | Pure; table-driven tests |
| Index diagrams | `brigid-core::diagrams` | Always sanitize/validate |
| Structural eval | `brigid-core::eval` | Fixtures under `tests/fixtures/tutorials/` |
| RunConfig | `brigid-core::config` | CLI > file > env > defaults |
| Checkpoint types | `brigid-core::checkpoint` | ADR 0001 metadata; ADR 0006 stage outputs |
| Progress / LLM budget | `brigid-core::progress` | Fail-closed max calls |
| Secrets redaction | `brigid-core::secrets` | Paths + content heuristics |
| i18n chrome | `brigid-core::i18n` | `Locale` + `ChromeStrings` (en/es); ADR 0007 |
| Chapter domain types | `brigid-core::chapter` | `Chapter`, `ChapterOrder`, `ChapterResult` |
| M4 domain types | `brigid-core::generate` | `SetupGuide`, `ArchitectureOverview`, `CombinedTutorial` |
| M4 domain types | `brigid-core::abstraction` | `RelationshipsResult`, `Relationship` |
| LLM disk cache | `brigid-llm::cache` | Hash-keyed response cache; enabled by default with LRU eviction (ADR 0009) |
| LLM provider client | `brigid-llm::openai_client` | OpenAI-compatible HTTP + retry/backoff + host allowlist |
| Bounded concurrency | `brigid-llm::concurrency` | Semaphore-gated map batches |
| Checkpoint store | `brigid-pipeline::checkpoint_store` | save/load bundle; file-based stage outputs (ADR 0006) |
| Resume helpers | `brigid-pipeline::resume` | stage-skip / invalidate |
| Local crawl | `brigid-crawl::local` | FS I/O; symlink cycle detection |
| Dry-run plan | `brigid-pipeline::dry_run` | Orchestration |
| Identify stage | `brigid-pipeline::identify` | Map/reduce + single-shot |
| Relationships stage | `brigid-pipeline::relationships` | Budgeted evidence selection |
| Order stage | `brigid-pipeline::order` | Chapter ordering + validation |
| Chapters stage | `brigid-pipeline::chapters` | Bounded-concurrent chapter writing |
| Setup guide stage | `brigid-pipeline::setup_guide` | Score-triggered generation |
| Overview stage | `brigid-pipeline::overview` | Multi-app architecture overview |
| Combine stage | `brigid-pipeline::combine` | Index + diagrams + i18n chrome + sanitize |
| Generate orchestration | `brigid-pipeline::generate` | Full pipeline + `--each-app` fan-out |
| Chapter review | `brigid-pipeline::review` | `--review-chapters` second LLM pass |
| Prompt rendering | `brigid-pipeline::prompts` | minijinja templates |
| Benchmarks | `brigid-pipeline::benches/` | criterion benchmarks for critical paths |
| CLI | `brigid-cli` | Thin wrappers + `assert_cmd` tests; completions + man page |

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

- **Library crates** (`brigid-core`, `brigid-crawl`, `brigid-llm`,
  `brigid-pipeline`) perform no CLI or main-binary logic and stay easy to unit
  test.
- **Public APIs must have rustdoc.** Each library crate declares
  `#![deny(missing_docs)]`.
- Prefer typed errors with `thiserror` in libraries; user-facing messages and
  exit codes live in `brigid-cli`.
- Use `clap` derive for CLI flags. Keep the binary a thin wrapper around the
  pipeline crates.
- All GitHub Actions we use are pinned by full commit SHA.

## Crate layout

```text
crates/
  brigid-core/      pure domain: models, traits, mermaid, budgeting
  brigid-crawl/     local + GitHub fetching
  brigid-llm/       provider clients, retries, caching
  brigid-pipeline/  stage orchestration, checkpoint/resume
  brigid-cli/       clap binary and exit codes
```

Add new crates under `crates/` and register them in the root `Cargo.toml`
workspace `members` list.

## Documentation

- User-facing docs live in [`README.md`](README.md) (overview + quick start),
  [`docs/usage-guide.md`](docs/usage-guide.md) (deep command reference),
  [`docs/troubleshooting.md`](docs/troubleshooting.md) (exit codes + recovery),
  and [`docs/project-status.md`](docs/project-status.md) (milestones + roadmap).
- Product quality rules live in [`docs/best-practices.md`](docs/best-practices.md).
- Migration design and phase plan live in
  [`docs/move-to-rust.md`](docs/move-to-rust.md).
- Crate structure, pipeline data flow, key types, and checkpoint/resume design
  are documented in [`ARCHITECTURE.md`](ARCHITECTURE.md).
- Release history is tracked in [`CHANGELOG.md`](CHANGELOG.md).
- Architecture decisions are recorded in `docs/adr/` when they affect the
  checkpoint format, crate boundaries, or provider contract.
- Every public function, type, and module should have a rustdoc comment.

## Commit messages

We follow [Conventional Commits](https://www.conventionalcommits.org/) so the
history is greppable and `CHANGELOG.md` entries are easy to assemble.

### Format

```
<type>: <description>

[optional body]

[optional footer(s)]
```

### Types

| Type | Use for |
|------|---------|
| `feat` | A new feature (user-visible) |
| `fix` | A bug fix (user-visible) |
| `docs` | Documentation-only changes |
| `refactor` | Code restructuring with no behavior change |
| `test` | Adding or correcting tests |
| `ci` | CI/build/release pipeline changes |
| `perf` | Performance improvement |
| `chore` | Maintenance (deps, formatting) with no product impact |

### Rules

- The **subject line** is lowercase, imperative, and ≤ 72 characters — no
  trailing period.
- The **body** (when present) explains *why*, not *what*, wrapped at ~72
  columns.
- Reference the issue in a **footer trailer** on its own line, e.g.
  `Closes #212`. Use `Closes`, `Fixes`, or `Resolves` so GitHub auto-closes
  the issue on merge.
- For work produced with AI assistance, add a **co-author trailer**:

  ```
  Generated with [Devin](https://devin.ai)

  Co-Authored-By: Devin <158243242+devin-ai-integration[bot]@users.noreply.github.com>
  ```

### Examples

```
feat(pipeline): add git-diff incremental file detection (--since)

Closes #224
```

```
fix(core): treat blank env var as unset in config layering

Regression from #198 — `config_from_env_map` now filters empty strings
before merging, matching the documented CLI > file > env > defaults
precedence.

Fixes #210
```

## Pull requests

1. Create a feature branch from `main`: `feature/#-short-name` (e.g.
   `feature/3-contributing-guide`).
2. Make focused, incremental commits following
   [Commit messages](#commit-messages).
3. Ensure `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`,
   `cargo llvm-cov --fail-under-lines 85`, `cargo audit`, `cargo deny check`,
   and the eval regression gate (`cargo run -p brigid-cli -- eval --out
   tests/fixtures/tutorials/good-mini --threshold 80`) pass locally.
4. Open a PR that references the issue number (e.g. `Closes #3`).
5. Wait for CI (Ubuntu + macOS + Windows matrix for test/clippy) and any
   automated `rs-guard` review.
6. Merge only when CI is green.

## Getting help

- **Bugs and feature requests** — open a [GitHub issue](https://github.com/igmarin/brigid/issues).
  Include the `brigid --version`, the command you ran, and the full stderr
  output (redact API keys).
- **Questions and discussion** — use [GitHub Discussions](https://github.com/igmarin/brigid/discussions)
  for "how do I…" questions that are not bugs.
- **Architecture questions** — check [`ARCHITECTURE.md`](ARCHITECTURE.md) and
  [`docs/adr/`](docs/adr/) first; most design decisions are recorded there.
