# Architecture

This document describes the crate-level structure, data flow, and key types of
`brigid`. It is the contributor-facing companion to the user-facing
[`README.md`](README.md) and the migration design in
[`docs/move-to-rust.md`](docs/move-to-rust.md). Architectural decisions are
recorded in [`docs/adr/`](docs/adr/) and referenced where relevant below.

---

## Crate dependency hierarchy

`brigid` is a Cargo workspace of five crates. Dependencies flow strictly
downward: the CLI binary depends on the pipeline; the pipeline depends on the
LLM client, the crawler, and core; core has no workspace dependencies and stays
pure for easy unit testing.

```mermaid
graph TD
    CLI["brigid-cli<br/>(thin binary: clap args, exit codes)"]
    Pipeline["brigid-pipeline<br/>(stage orchestration, checkpoint/resume, dry-run)"]
    LLM["brigid-llm<br/>(LlmClient trait, provider clients, cache, retries)"]
    Crawl["brigid-crawl<br/>(local filesystem inventory and git-diff filtering)"]
    Core["brigid-core<br/>(pure domain: models, budget, mermaid, eval, i18n)"]

    CLI --> Pipeline
    CLI --> LLM
    CLI --> Crawl
    CLI --> Core
    Pipeline --> LLM
    Pipeline --> Crawl
    Pipeline --> Core
    LLM --> Core
    Crawl --> Core
```

The layering rule: **library crates perform no CLI or main-binary logic**.
`brigid-cli` is a thin wrapper that parses arguments, wires the pipeline, and
maps errors to exit codes. Public APIs in library crates carry rustdoc
(`#![deny(missing_docs)]`).

### Workspace layout

```
brigid/
├── crates/
│   ├── brigid-core/       # Pure domain models, traits, budgeting, mermaid sanitize
│   ├── brigid-crawl/      # Local filesystem inventory and git-diff filtering
│   ├── brigid-llm/        # LlmClient trait, provider clients, disk cache, retries
│   ├── brigid-pipeline/   # Stage orchestration, checkpoint/resume, dry-run, benchmarks
│   │   └── prompts/        # 10 versioned Jinja2 templates (identify, relationships, chapters, …)
│   ├── brigid-cli/        # Thin binary — clap args, completions, man page, exit codes
│   └── (brigid-mcp/)      # MCP server — proposed (ADR 0015), post-v1.0.0
├── tests/fixtures/       # Minimal repos + frozen baseline.json + Rust regenerator
├── docs/
│   ├── move-to-rust.md   # Migration design: pipeline model, domain objects, phase plan
│   ├── best-practices.md # Language-agnostic product rules (scope, budget, quality, mermaid)
│   └── adr/              # Architecture Decision Records (0001–0017)
├── homebrew/             # Homebrew formula template (brigid.rb)
├── .github/workflows/    # CI (fmt/clippy/test/cov/doc/audit/baseline) + release + rs-guard review
└── CONTRIBUTING.md       # TDD workflow, coverage gate, check commands
```

---

## Design principles

The architecture follows four principles that guide every stage and module
boundary:

### 1. I/O isolation

All filesystem, network, and subprocess I/O lives at the edges —
`brigid-crawl` (filesystem walk, `git` shell-out), `brigid-llm` (HTTP provider
calls, disk cache), and `brigid-cli` (argument parsing, exit codes). The
pipeline orchestration (`brigid-pipeline`) coordinates stages but delegates
I/O to these crates. `brigid-core` is **pure**: no network, no filesystem,
no `tokio`. This keeps the domain logic fast, deterministic, and easy to
unit-test without mocks.

### 2. Fail-closed budgeting

The `max_llm_calls` budget (`brigid-core::progress`) is enforced
**fail-closed**: if the tracker cannot confirm remaining budget, the stage
aborts rather than exceeding the limit. This prevents runaway costs on large
monorepos and surfaces budget exhaustion as a distinct exit code (3) instead
of a silent hang. The same principle applies to the context budget
(`brigid-core::budget`) — per-file truncation and per-batch char budgets are
hard limits, not suggestions.

### 3. Checkpoint-first

Every expensive stage writes its output to the checkpoint **before** the
stage is marked complete. On resume, each stage checks `completed_stages`
and skips if already done. Stage outputs are stored as files with SHA-256
verification (ADR 0006) so corruption is detected, not silently propagated.
Ctrl+C triggers a graceful shutdown that dumps a clean checkpoint (exit 5),
so the next run resumes from the last completed stage — never from a
half-written state.

### 4. Pure core

Domain types (`Abstraction`, `Relationship`, `Chapter`, `RunConfig`,
`CheckpointV1`, …) and all pure logic (budgeting, scope filtering, mermaid
sanitize, eval scoring, kind detection heuristics) live in `brigid-core`.
Stages in `brigid-pipeline` are thin orchestrators that call into core for
the actual work. This separation means the hard IP — quality rules,
monorepo heuristics, prompt contracts — is testable without any I/O and
reusable by future front-ends (a hosted mode, an editor plugin) without
dragging in the CLI.

---

## Pipeline data flow

The full `brigid generate` pipeline runs a linear sequence of stages. Every
expensive stage is idempotent and checkpointed so a long monorepo run can
resume after failure. The `dry-run` stage is zero-LLM and optional; it produces
a machine-readable plan for CI and agents.

```mermaid
flowchart LR
    crawl["crawl<br/>(fetch + scope)"]
    dryrun["dry-run<br/>(optional, zero-LLM)"]
    identify["identify<br/>(map/reduce)"]
    relationships["relationships<br/>(budgeted evidence)"]
    order["order<br/>(chapter ordering + validation)"]
    chapters["chapters<br/>(bounded-concurrent write)"]
    setup["setup<br/>(score-triggered)"]
    overview["overview<br/>(multi-app architecture)"]
    combine["combine<br/>(index + diagrams + i18n)"]
    eval["eval<br/>(structural quality gate)"]

    crawl --> dryrun
    crawl --> identify
    dryrun --> identify
    identify --> relationships
    relationships --> order
    order --> chapters
    chapters --> setup
    chapters --> overview
    setup --> combine
    overview --> combine
    combine --> eval
```

`setup` and `overview` are conditional: the setup guide is generated only when
the README/config setup score is weak; the architecture overview is generated
only for multi-app (monorepo) systems. With `--each-app`, the pipeline fans out
and runs the stages per app, then combines per-app tutorials.

### Stage ordering and resume

Stages are tracked by the `StageId` enum (`fetch`, `dry_run`, `identify`,
`relationships`, `order`, `chapters`, `setup`, `overview`, `combine`, `eval`).
On resume, each stage checks the checkpoint for completion and skips if already
done, enabling resume from any point without re-running expensive LLM calls.

---

## Module responsibilities

| Crate | Module | Responsibility |
|-------|--------|----------------|
| `brigid-core` | `module` | Module keys and inventory discovery (pure) |
| `brigid-core` | `scope` | Monorepo `--apps` / `--exclude-apps` filter (pure) |
| `brigid-core` | `setup` | Setup assessment scoring from five README signals (pure) |
| `brigid-core` | `budget` | Context budget: per-file truncate, per-batch char budget (pure) |
| `brigid-core` | `mermaid` | Mermaid sanitize and light validate (pure) |
| `brigid-core` | `diagrams` | Deterministic index diagram builders (always sanitize/validate) |
| `brigid-core` | `eval` | Structural tutorial quality gate (fixtures under `tests/fixtures/tutorials/`) |
| `brigid-core` | `config` | `RunConfig` layering: CLI > file > env > defaults |
| `brigid-core` | `checkpoint` | Checkpoint schema v1 types (ADR 0001 metadata; ADR 0006 stage outputs) |
| `brigid-core` | `progress` | Progress tracker and max-LLM-calls budget (fail-closed) |
| `brigid-core` | `secrets` | Secrets path classification and content redaction |
| `brigid-core` | `i18n` | `Locale` + `ChromeStrings` (en/es); ADR 0007 |
| `brigid-core` | `chapter` | `Chapter`, `ChapterOrder`, `ChapterResult` domain types |
| `brigid-core` | `abstraction` | `Abstraction`, `Relationship`, `IdentifyResult`, `RelationshipsResult` |
| `brigid-core` | `generate` | `SetupGuide`, `ArchitectureOverview`, `CombinedTutorial` |
| `brigid-core` | `extract` | Robust YAML/JSON block extraction from messy LLM output |
| `brigid-core` | `stage_output` | `StageOutput<T>` JSON envelope and per-stage output types (ADR 0012) |
| `brigid-core` | `plugin` | `KindDetector` trait, `PluginRegistry`, `DefaultKindDetector` (ADR 0014) |
| `brigid-crawl` | `local` | Local filesystem inventory (hidden-directory skipping and symlink cycle detection; `.gitignore` support is not yet implemented) |
| `brigid-crawl` | `git_diff` | Git-diff incremental file detection via `git` shell-out; `--since` support (ADR 0013) |
| `brigid-llm` | `client` | `LlmClient` async trait (ADR 0002) |
| `brigid-llm` | `mock` | `MockClient` test double |
| `brigid-llm` | `openai_client` | OpenAI-compatible HTTP client with retry/backoff/timeout |
| `brigid-llm` | `cache` | Disk response cache keyed by hash(prompt)+model+provider; enabled by default with LRU eviction (ADR 0009) |
| `brigid-llm` | `concurrency` | Bounded-concurrency map batches via `tokio::sync::Semaphore` (ADR 0003) |
| `brigid-pipeline` | `dry_run` | Dry-run plan assembly with baseline parity |
| `brigid-pipeline` | `identify` | Map/reduce + single-shot identify stages |
| `brigid-pipeline` | `identify_checkpoint` | Checkpoint-after-identify and resume |
| `brigid-pipeline` | `relationships` | Budgeted evidence selection |
| `brigid-pipeline` | `order` | Chapter ordering + validation |
| `brigid-pipeline` | `chapters` | Bounded-concurrent chapter writing |
| `brigid-pipeline` | `setup_guide` | Score-triggered setup guide generation |
| `brigid-pipeline` | `overview` | Multi-app architecture overview |
| `brigid-pipeline` | `combine` | Index + diagrams + i18n chrome + sanitize |
| `brigid-pipeline` | `review` | `--review-chapters` second LLM pass per chapter |
| `brigid-pipeline` | `generate` | Full pipeline orchestration + `--each-app` fan-out |
| `brigid-pipeline` | `checkpoint_store` | Save/load bundle; file-based stage outputs (ADR 0006) |
| `brigid-pipeline` | `resume` | Stage-skip / invalidate helpers |
| `brigid-pipeline` | `prompts` | `minijinja` prompt rendering |
| `brigid-pipeline` | `cancellation` | Ctrl+C / SIGTERM graceful shutdown (exit 5) |
| `brigid-cli` | `main` | Clap argument parsing, pipeline wiring, exit codes |

---

## Key types and relationships

The pipeline passes domain objects between stages. All types live in
`brigid-core` so library crates stay testable without network or filesystem
dependencies.

```mermaid
classDiagram
    class Abstraction {
        +name: String
        +description: String
        +file_indices: Vec~usize~
        +tier: Tier
        +kind: AbstractionKind
        +apps: Vec~String~
        +entry_files: Vec~String~
    }
    class Relationship {
        +from: usize
        +to: usize
        +label: String
        +kind: String
    }
    class IdentifyResult {
        +abstractions: Vec~Abstraction~
        +relationships: Vec~Relationship~
    }
    class RelationshipsResult {
        +relationships: Vec~Relationship~
        +evidence: Vec~String~
    }
    class ChapterOrder {
        +ordered: Vec~usize~
    }
    class Chapter {
        +abstraction_index: usize
        +title: String
        +body: String
    }
    class ChapterResult {
        +chapters: Vec~Chapter~
    }
    class SetupGuide {
        +markdown: String
    }
    class ArchitectureOverview {
        +markdown: String
    }
    class CombinedTutorial {
        +index: String
        +chapters: Vec~Chapter~
        +setup: Option~SetupGuide~
        +overview: Option~ArchitectureOverview~
    }

    IdentifyResult o-- Abstraction
    IdentifyResult o-- Relationship
    RelationshipsResult o-- Relationship
    ChapterResult o-- Chapter
    CombinedTutorial o-- Chapter
    CombinedTutorial o-- SetupGuide
    CombinedTutorial o-- ArchitectureOverview
```

| Type | Crate / module | Produced by | Consumed by |
|------|----------------|-------------|-------------|
| `Abstraction` | `brigid-core::abstraction` | identify (map/reduce) | relationships, order, chapters |
| `Relationship` | `brigid-core::abstraction` | identify, relationships | combine (index diagrams) |
| `IdentifyResult` | `brigid-core::abstraction` | identify stage | relationships, order, chapters |
| `RelationshipsResult` | `brigid-core::abstraction` | relationships stage | order, combine |
| `ChapterOrder` | `brigid-core::chapter` | order stage | chapters |
| `Chapter` | `brigid-core::chapter` | chapters stage | combine |
| `ChapterResult` | `brigid-core::chapter` | chapters stage | combine, review |
| `SetupGuide` | `brigid-core::generate` | setup stage (score-triggered) | combine |
| `ArchitectureOverview` | `brigid-core::generate` | overview stage (multi-app) | combine |
| `CombinedTutorial` | `brigid-core::generate` | combine stage | eval, output write |
| `RunConfig` | `brigid-core::config` | CLI / file / env layering | every stage |
| `CheckpointV1` | `brigid-core::checkpoint` | checkpoint store | resume, every stage |

---

## Checkpoint and resume architecture

Long monorepo runs can require dozens of LLM calls. Every expensive stage is
idempotent and checkpointed so the pipeline can resume after failure or
Ctrl+C interruption.

### Format (ADR 0001)

The checkpoint is split into a small metadata file and a compressed sidecar:

- `checkpoint.json` — versioned metadata: `completed_stages`, `config`,
  `abstractions`, `relationships`, and a manifest pointer.
- `files.ndjson.gz` — the crawled file corpus as content-addressed records,
  kept out of the metadata file so storage and memory stay bounded as the
  repository grows.

Storing full file bodies inside one giant JSON checkpoint does not scale; the
split design avoids the I/O bottleneck the Python reference suffered from. See
[ADR 0001](docs/adr/0001-checkpoint-schema-v1.md).

### File-based stage output storage (ADR 0006)

M4 stages produce large Markdown documents (one chapter file per abstraction,
a setup guide, an architecture overview, a combined index). Storing these as
inline JSON strings inside `checkpoint.json` would recreate the original
problem ADR 0001 solved for file bodies. Instead, stage outputs are written as
files alongside the checkpoint with SHA-256 verification, so the metadata file
stays small and stage save/load remains fast. See
[ADR 0006](docs/adr/0006-file-based-checkpoint-output-storage.md).

### Resume behavior

On resume, each stage checks `completed_stages` in the checkpoint and skips if
already done. The `resume` module provides stage-skip and partial-regenerate
helpers. `brigid resume --checkpoint PATH` reports the next pending stage
without re-running anything. Ctrl+C triggers a graceful shutdown
(`cancellation` module) that dumps a clean checkpoint and exits with code 5,
so the next run resumes from the last completed stage.

---

## Architectural decisions

Architectural decisions are recorded as ADRs in [`docs/adr/`](docs/adr/).

| ADR | Decision |
|-----|----------|
| [0001](docs/adr/0001-checkpoint-schema-v1.md) | Content-addressed checkpoint format for resume |
| [0002](docs/adr/0002-async-trait-llm-client.md) | `async-trait` for the `LlmClient` trait (object-safety tradeoff) |
| [0003](docs/adr/0003-bounded-concurrency-semaphore.md) | `tokio::sync::Semaphore` for bounded map batches |
| [0004](docs/adr/0004-yaml-parser-migration.md) | Migration off unsound `serde_yml`/`libyml` (superseded by 0005) |
| [0005](docs/adr/0005-yaml-parser-serde-yaml-ng.md) | Migration to the maintained `serde_yaml_ng` fork |
| [0006](docs/adr/0006-file-based-checkpoint-output-storage.md) | File-based stage output storage with SHA-256 verification |
| [0007](docs/adr/0007-i18n-chrome-design.md) | Generic localization framework for tutorial chrome strings |
| [0008](docs/adr/0008-two-tier-golden-fixture-strategy.md) | Two-tier golden fixture strategy for eval regression |
| [0009](docs/adr/0009-disk-cache-default-lru-eviction.md) | Disk cache enabled by default with LRU eviction and size limits |
| [0010](docs/adr/0010-release-strategy.md) | Release strategy: GitHub Releases, Homebrew, cargo install, cargo-binstall |
| [0011](docs/adr/0011-python-deprecation-approach.md) | Python deprecation: migration guide over wrapper (Option B) |
| [0012](docs/adr/0012-json-output-schema.md) | JSON output schema for pipeline stages (`StageOutput<T>` envelope) |
| [0013](docs/adr/0013-git-diff-incremental.md) | Git-diff incremental file detection (`--since`) |
| [0014](docs/adr/0014-plugin-architecture.md) | Plugin trait and registry for custom kind detectors |
| [0015](docs/adr/0015-mcp-server.md) | MCP server for codebase knowledge querying (proposed, post-v1.0.0) |
| [0016](docs/adr/0016-graph-provider.md) | Graph provider trait for structural ground truth from codegraph/Graphify (proposed, post-v1.0.0) |
| [0017](docs/adr/0017-openrouter-provider.md) | OpenRouter as a first-class LLM provider |

---

## Related documentation

- [`README.md`](README.md) — project overview, quick start, CLI at a glance
- [`docs/usage-guide.md`](docs/usage-guide.md) — every command, flag, env var,
  provider setup, examples
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — exit codes, recovery,
  common issues
- [`docs/project-status.md`](docs/project-status.md) — milestones, what works,
  roadmap
- [`docs/move-to-rust.md`](docs/move-to-rust.md) — migration design, pipeline
  model, domain objects, phase plan, engineering bar
- [`docs/best-practices.md`](docs/best-practices.md) — language-agnostic
  product rules: scope, budget, abstraction quality, mermaid, setup docs
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — TDD workflow, coverage gate, CI
  checks, PR process, commit conventions
- [`CHANGELOG.md`](CHANGELOG.md) — release history (Keep a Changelog format)
- [`crates/brigid-pipeline/prompts/README.md`](crates/brigid-pipeline/prompts/README.md) — prompt catalog and variable schema
- [`tests/fixtures/README.md`](tests/fixtures/README.md) — fixture set and
  parity strategy
