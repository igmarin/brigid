# decon

> Deconstruct massive code monoliths into structured, beginner-friendly tutorials — powered by LLMs, built in Rust.

`decon` crawls a codebase (local tree or GitHub URL), identifies its core
abstractions, and produces a multi-chapter Markdown + Mermaid tutorial that
explains how the system works — including setup, architecture, and
inter-concept relationships. It is designed for monorepos and large codebases
where "read the source" is not a realistic onboarding path.

This is a **Rust rewrite** of a Python/PocketFlow reference implementation. The
product value lives in the pipeline stages, prompt catalog, and quality
heuristics — not the runtime. The Python entrypoint is now **deprecated**; see
[`docs/migrating-from-python.md`](docs/migrating-from-python.md) for the
migration guide. See [`docs/move-to-rust.md`](docs/move-to-rust.md) for the
full migration design.

---

## Current status

**Milestone 4 (Full Generate) — complete.**
The full `decon generate` pipeline (relationships → order → chapters → setup →
overview → combine) is working, with i18n chrome (English + Spanish),
`--each-app` monorepo fan-out, `--review-chapters` polishing, file-based
checkpoint output storage, and an eval regression CI gate. M5 (product polish)
is next.

| Milestone | Goal | Status |
|-----------|------|--------|
| **M0** — Spec Freeze | Workspace layout, CI, CONTRIBUTING, ADR 0001, prompt catalog, test fixtures, parity baseline | ✅ Done |
| **M1** — Crawl + Dry-run + Eval | `decon crawl` / dry-run matching `baseline.json`; mermaid sanitize; setup-assessment parity; `decon eval` port | ✅ Done |
| **M2** — Checkpoint, Config & Coverage | Content-addressed checkpoint (ADR 0001); `decon.toml`; ≥85% coverage gate | ✅ Done |
| **M3** — LLM Identify | `LlmClient` trait + provider clients; map/reduce identify; checkpoint resume; Ctrl+C graceful shutdown | ✅ Done |
| **M4** — Full Generate | Relationships → order → chapters → setup → overview → combine; Spanish chrome; `--each-app`; `--review-chapters`; eval regression gate | ✅ Done |
| **M5** — Product Polish | Installers, man page, shell completions, concurrency, error UX, Python deprecation | 🔜 In progress |

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
- **CLI (M3):**
  - `decon identify --dir PATH [--checkpoint-dir PATH]` — map/reduce identify
    with checkpoint resume and Ctrl+C graceful shutdown
- **CLI (M4):**
  - `decon generate --dir PATH [--output-dir PATH] [--language en|es]
    [--each-app] [--review-chapters]` — full pipeline: identify → relationships
    → order → chapters → setup → overview → combine
  - Per-stage subcommands for debugging: `decon relationships`, `decon order`,
    `decon chapters`, `decon setup`, `decon overview`, `decon combine`
  - `--each-app` flag for per-app tutorial generation in monorepos
  - `--review-chapters` flag for optional chapter quality polishing (second LLM
    pass per chapter)
  - i18n chrome: `--language es` localizes index/footer headings and labels
    (English + Spanish locales)
- **`decon-core` pure helpers:** module keys, monorepo scope, setup scoring,
  context budget, Mermaid sanitize/validate, index diagram builders, structural
  eval, `RunConfig` layering, checkpoint schema v1 types, progress budget,
  secrets redaction, i18n chrome (`Locale`, `ChromeStrings`), chapter domain
  types (`Chapter`, `ChapterOrder`, `ChapterResult`), M4 domain types
  (`RelationshipsResult`, `SetupGuide`, `ArchitectureOverview`,
  `CombinedTutorial`).
- **Checkpoint store + resume** (`decon-pipeline`): `checkpoint.json` +
  `files.ndjson.gz`, stage-skip / partial regenerate helpers, file-based stage
  output storage with SHA-256 verification (ADR 0006).
- **LLM provider client** (`decon-llm`): `OpenAiCompatibleClient` with
  retry/backoff/timeout, host allowlist validation, disk cache
  (key = hash(prompt)+model+provider), and bounded-concurrency map batches.
- **CI coverage hard gate:** ≥85% workspace line coverage.
- **Parity fixtures:** `tests/fixtures/{python-lib,umbrella,js-lib}` + frozen
  `baseline.json`; tutorial goldens under `tests/fixtures/tutorials/`
  (hand-crafted `good-mini` + `broken-mini`, LLM-generated `llm-generated`).
- **CI pipeline:** fmt, clippy (`-D warnings`), test, coverage report, doc,
  `cargo audit`, `cargo deny check`, fixture baseline check, eval regression
  gate (good-mini + broken-mini), nightly LLM smoke, rs-guard PR review.
- **Prompt catalog** (`prompts/`) and **ADR 0001** checkpoint schema (used from M2+).

### Usage examples

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
cargo run -p decon-cli -- resume --checkpoint /tmp/decon-demo --format json

# Full generate pipeline (requires DEEPSEEK_API_KEY or DECON_LLM_API_KEY; see
# [API key setup](#api-key-setup) below)
cargo run -p decon-cli -- generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorial --language en

# Generate per-app tutorials in a monorepo
cargo run -p decon-cli -- generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorials --each-app

# Generate with Spanish chrome and chapter review
cargo run -p decon-cli -- generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorial --language es --review-chapters

# Run a single stage for debugging
cargo run -p decon-cli -- relationships --dir tests/fixtures/umbrella \
  --checkpoint-dir /tmp/checkpoint
```

### What does not work yet

Remaining M5 items: concurrency limits and improved error UX. Native
installers (Homebrew, `cargo install`, GitHub Releases), the man page, and
shell completions are now available — see
[Installation](#installation) below. The Python entrypoint has been deprecated
— see [`docs/migrating-from-python.md`](docs/migrating-from-python.md) for the
migration guide.

---

## Installation

`decon` is distributed as pre-built binaries for Linux (x86_64, aarch64),
macOS (x86_64, aarch64), and Windows (x86_64). You can install it via
Homebrew, `cargo install`, `cargo-binstall`, or by downloading a binary
directly from GitHub Releases.

### Homebrew (macOS)

```bash
brew tap igmarin/homebrew-tap
brew install decon
```

This installs the `decon` binary, man page (`man 1 decon`), and shell
completions (bash, zsh, fish) automatically.

### cargo install

```bash
cargo install decon-cli
```

Or install directly from the git repository:

```bash
cargo install --git https://github.com/igmarin/decon-rs decon-cli
```

### cargo-binstall (fast binary download)

If you have [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall)
installed, `decon-cli` ships with binstall metadata so it downloads a
pre-built binary instead of compiling from source:

```bash
cargo binstall decon-cli
```

### Direct download

1. Go to the [Releases page](https://github.com/igmarin/decon-rs/releases).
2. Download the archive matching your platform, e.g.
   `decon-0.1.0-x86_64-unknown-linux-gnu.tar.gz`.
3. Verify the SHA-256 checksum against the `SHA256SUMS` file in the release.
4. Extract the archive and move the `decon` binary to your `PATH`:

   ```bash
   tar xzf decon-0.1.0-x86_64-unknown-linux-gnu.tar.gz
   sudo mv decon-0.1.0-x86_64-unknown-linux-gnu/decon /usr/local/bin/
   # Optional: install man page and completions
   sudo mv decon-0.1.0-x86_64-unknown-linux-gnu/decon.1 /usr/local/share/man/man1/
   mkdir -p ~/.local/share/bash-completion/completions
   mv decon-0.1.0-x86_64-unknown-linux-gnu/completions/decon.bash \
      ~/.local/share/bash-completion/completions/decon
   ```

5. Verify: `decon --version`

### Verifying checksums

Every release includes a `SHA256SUMS` file listing the SHA-256 hash of each
artifact. Verify a downloaded archive before installing:

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

---

## Shell completions

`decon` can generate completion scripts for bash, zsh, fish, and PowerShell.
The scripts cover every subcommand and flag automatically.

```bash
# Print a script to stdout
decon completions --shell bash
decon completions --shell zsh
decon completions --shell fish
decon completions --shell powershell

# Write directly to a file
decon completions --shell bash --output ~/.decon-completions.bash
```

### Installation

**bash** — install into the system completions directory (or source it from
your `.bashrc`):

```bash
decon completions --shell bash --output /etc/bash_completion.d/decon
# or, for a user-level install:
decon completions --shell bash --output ~/.local/share/bash-completion/completions/decon
```

**zsh** — place the script on your `$fpath` (commonly `~/.zsh/completions`):

```bash
mkdir -p ~/.zsh/completions
decon completions --shell zsh --output ~/.zsh/completions/_decon
```

Make sure `~/.zsh/completions` is on your `fpath` (add
`fpath=(~/.zsh/completions $fpath)` to `~/.zshrc`) and run `compinit`.

**fish** — drop the script into the fish completions directory:

```bash
mkdir -p ~/.config/fish/completions
decon completions --shell fish --output ~/.config/fish/completions/decon.fish
```

**PowerShell** — add the script to your PowerShell profile:

```powershell
decon completions --shell powershell --output $PROFILE
# or append to an existing profile:
decon completions --shell powershell | Add-Content $PROFILE
```

Reload your shell (or open a new terminal) after installing.

---

## Man page

`decon` can generate a troff-formatted man page covering every subcommand and
flag. The man page includes SYNOPSIS, DESCRIPTION, OPTIONS, COMMANDS,
EXAMPLES, ENVIRONMENT, FILES, EXIT STATUS, and SEE ALSO sections.

```bash
# Print the man page to stdout
decon manpage

# Write it to a file and install it
decon manpage > decon.1 && sudo mv decon.1 /usr/local/share/man/man1/

# Or write directly to a file with --output
decon manpage --output decon.1
man -l decon.1
```

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
├── homebrew/             # Homebrew formula template (decon.rb)
├── .github/workflows/    # CI (fmt/clippy/test/cov/doc/audit/baseline) + release + rs-guard review
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

## API key setup

The `generate` and `identify` stages call an LLM. `decon` uses an
OpenAI-compatible HTTP client, so any provider that exposes that API works.

### DeepSeek (default / primary)

DeepSeek is the default provider — no extra configuration beyond the API key.

1. Create an account at <https://platform.deepseek.com> and generate an API
   key.
2. Export it before running `decon`:

   ```bash
   export DEEPSEEK_API_KEY="sk-your-key-here"
   # or use the generic name (checked first):
   export DECON_LLM_API_KEY="sk-your-key-here"
   ```

`DECON_LLM_API_KEY` takes precedence over `DEEPSEEK_API_KEY`; either is
accepted. The key is **never** written to `decon.toml` — only read from the
environment.

### OpenAI

Point the client at the OpenAI endpoint and pick a model:

```bash
export DECON_LLM_API_KEY="sk-your-openai-key"
export DECON_LLM_BASE_URL="https://api.openai.com/v1"
export DECON_LLM_MODEL="gpt-4o"
```

`api.openai.com` is in the built-in host allowlist, so no extra host
configuration is needed.

### Local providers (Ollama, LM Studio)

Any OpenAI-compatible local server works. Set the base URL to the local
endpoint and add the host to the allowlist if it is not already covered by
the `localhost` / `127.0.0.1` defaults.

```bash
# Ollama (default port 11434)
export DECON_LLM_API_KEY="ollama"          # any non-empty string
export DECON_LLM_BASE_URL="http://localhost:11434/v1"
export DECON_LLM_MODEL="llama3"

# LM Studio (default port 1234)
export DECON_LLM_API_KEY="lm-studio"
export DECON_LLM_BASE_URL="http://localhost:1234/v1"
export DECON_LLM_MODEL="local-model"
```

For a non-loopback host, extend the allowlist with
`DECON_LLM_ALLOWED_HOSTS` (comma-separated) or the `[[allowed_hosts]]` table
in `decon.toml`:

```bash
export DECON_LLM_ALLOWED_HOSTS="my-proxy.internal,10.0.0.5"
```

### Relevant environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `DECON_LLM_API_KEY` | — | API key (checked first; falls back to `DEEPSEEK_API_KEY`) |
| `DEEPSEEK_API_KEY` | — | API key (fallback) |
| `DECON_LLM_BASE_URL` | `https://api.deepseek.com/v1` | OpenAI-compatible endpoint |
| `DECON_LLM_MODEL` | `deepseek-chat` | Model identifier sent in requests |
| `DECON_LLM_ALLOWED_HOSTS` | — | Extra hosts for the Authorization-header allowlist (comma-separated) |
| `DECON_LLM_CACHE_DIR` | platform cache `/decon/llm-cache` | Disk cache root for LLM responses |
| `DECON_NO_CACHE` | — | Set to `1` / `true` to disable the disk cache |
| `DECON_FORCE_MOCK` | — | Set to any non-empty value to force the mock client (offline) |

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
# Eval regression gate (good-mini passes, broken-mini fails at threshold 80)
cargo run -p decon-cli -- eval --out tests/fixtures/tutorials/good-mini --threshold 80
```

CI also runs a **nightly LLM smoke** job (scheduled, not on PR/push) that
generates a tutorial with a live DeepSeek key, evals the output, and compares
the score against the frozen `llm-generated` fixture — opening a GitHub issue
on regression.

A **release workflow** (`.github/workflows/release.yml`) triggers on tag push
(`vX.Y.Z`), builds release binaries for Linux (x86_64, aarch64), macOS
(x86_64, aarch64), and Windows (x86_64), packages them with the man page and
completion scripts, generates SHA-256 checksums, and creates a GitHub Release
with notes extracted from [`CHANGELOG.md`](CHANGELOG.md). It can also be
dispatched manually in dry-run mode to validate the build without publishing.

---

## Troubleshooting

### Exit codes

`decon` maps outcomes to stable exit codes so CI and scripts can branch on
them:

| Code | Meaning | What to do |
|------|---------|------------|
| `0` | Success | — |
| `1` | Generic failure | Check stderr for the error message; usually an unexpected pipeline error |
| `2` | Config / path / I/O error | Verify `--dir` exists, `decon.toml` is valid TOML, checkpoint path is correct |
| `3` | Budget exhausted | The `max_llm_calls` limit was hit; raise it in `decon.toml` |
| `4` | LLM provider error | Network, timeout, rate-limit, or parse error from the provider; see [LLM provider issues](#llm-provider-issues) |
| `5` | Cancelled (Ctrl+C / SIGTERM) | A partial checkpoint was saved; re-run the same command to resume |

### Checkpoint recovery

Every expensive stage is checkpointed. If a run is interrupted (exit 5) or
fails (exit 1/3/4), re-running the same command resumes from the last
completed stage — completed stages are skipped automatically.

To inspect progress without re-running anything:

```bash
decon resume --checkpoint /path/to/checkpoint-dir --format json
```

The checkpoint lives in the `--checkpoint-dir` (default: a temp dir under the
output directory) and consists of `checkpoint.json` + `files.ndjson.gz`. See
[ADR 0001](docs/adr/0001-checkpoint-schema-v1.md) and
[ADR 0006](docs/adr/0006-file-based-checkpoint-output-storage.md) for the
format. To start fresh, delete the checkpoint directory and re-run.

### LLM provider issues

- **`DECON_LLM_API_KEY (or DEEPSEEK_API_KEY) not set`** — No API key found.
  See [API key setup](#api-key-setup). Without a key, `decon` falls back to a
  mock client (useful for offline tests, not for real generation).
- **`host '…' is not in the allowed hosts list`** — The `base_url` host is not
  approved to receive the `Authorization` header. Add it via
  `DECON_LLM_ALLOWED_HOSTS` or the `[[allowed_hosts]]` table in `decon.toml`.
- **Rate limits / timeouts (exit 4)** — The client retries with backoff, but
  sustained rate limiting will surface as exit 4. Wait and retry, or switch to
  a provider/model with a higher rate limit.
- **`DECON_FORCE_MOCK`** — Setting this to any non-empty value forces the mock
  client even when a real key is present, for offline reproducibility.

### Cache problems

LLM responses are cached on disk (keyed by hash(prompt)+model+provider) so
re-runs with an unchanged prompt are free.

- **Stale / wrong responses** — Clear the cache by deleting the cache dir
  (default: platform cache `/decon/llm-cache`) or set `DECON_NO_CACHE=1` to
  bypass it for a single run.
- **Custom cache location** — Set `DECON_LLM_CACHE_DIR=/some/path`.
- **Disk full** — The cache enforces a size limit (default 100 MB) and evicts
  oldest entries; if writes fail, check permissions and free space.

### Budget exhaustion (exit 3)

The `max_llm_calls` budget caps total LLM calls per run (fail-closed). If a
large monorepo run hits the limit mid-pipeline:

1. Check the checkpoint with `decon resume` to see which stages completed.
2. Raise the budget in `decon.toml` (`max_llm_calls = 500`) or via CLI flag.
3. Re-run the same command — completed stages are skipped, so only the
   remaining calls count against the new budget.

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
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Crate structure, pipeline data flow, key types, checkpoint/resume design, ADR index |
| [`CHANGELOG.md`](CHANGELOG.md) | Release history per milestone (Keep a Changelog format) |
| [`docs/move-to-rust.md`](docs/move-to-rust.md) | Migration design, pipeline model, domain objects, phase plan, engineering bar |
| [`docs/migrating-from-python.md`](docs/migrating-from-python.md) | Migration guide for Python `decon` users: command mapping, env vars, feature parity, FAQ |
| [`docs/best-practices.md`](docs/best-practices.md) | Language-agnostic product rules: scope, budget, abstraction quality, mermaid, setup docs |
| [`docs/adr/0001-checkpoint-schema-v1.md`](docs/adr/0001-checkpoint-schema-v1.md) | Content-addressed checkpoint format for resume |
| [`docs/adr/0002-async-trait-llm-client.md`](docs/adr/0002-async-trait-llm-client.md) | Why `async-trait` for `LlmClient` (object-safety tradeoff) |
| [`docs/adr/0003-bounded-concurrency-semaphore.md`](docs/adr/0003-bounded-concurrency-semaphore.md) | Why `tokio::sync::Semaphore` for bounded map batches |
| [`docs/adr/0004-yaml-parser-migration.md`](docs/adr/0004-yaml-parser-migration.md) | Migration off unsound `serde_yml`/`libyml` (superseded by 0005) |
| [`docs/adr/0005-yaml-parser-serde-yaml-ng.md`](docs/adr/0005-yaml-parser-serde-yaml-ng.md) | Migration to `serde_yaml_ng` (maintained fork) |
| [`docs/adr/0006-file-based-checkpoint-output-storage.md`](docs/adr/0006-file-based-checkpoint-output-storage.md) | File-based stage output storage with SHA-256 verification |
| [`docs/adr/0007-i18n-chrome-design.md`](docs/adr/0007-i18n-chrome-design.md) | Generic localization framework for tutorial chrome strings |
| [`docs/adr/0008-two-tier-golden-fixture-strategy.md`](docs/adr/0008-two-tier-golden-fixture-strategy.md) | Two-tier golden fixture strategy for eval regression |
| [`prompts/README.md`](prompts/README.md) | Prompt catalog: 10 templates, variable schema, integration notes |
| [`tests/fixtures/README.md`](tests/fixtures/README.md) | Fixture set, baseline regenerator, parity strategy |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | TDD workflow, coverage gate, CI checks, PR process |

---

## License

MIT
