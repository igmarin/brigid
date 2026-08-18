# Changelog

All notable changes to `brigid` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are written retroactively from the merged pull requests and milestone
descriptions in [`docs/move-to-rust.md`](docs/move-to-rust.md). Each milestone
corresponds to a minor release; the workspace `version` field in `Cargo.toml`
tracks the latest.

## [Unreleased]

### Cache management (issue #300)

- **Added `brigid cache prune` subcommand**: deletes the SQLite cache database
  and its WAL/SHM sidecars. Replaces the previous manual `rm -rf` workaround.
- **Added `brigid cache stats` subcommand**: prints the cache file path, entry
  count, and on-disk size (including WAL/SHM sidecars).
- **Added `brigid_pipeline::StatsClient`**: an `LLMClient` wrapper that tracks
  cache hit/miss statistics by probing the `KvStore` before each call. Exports
  `CacheStats` (hits, misses, hit_rate_percent) and `StatsClient` from
  `brigid_pipeline`. This restores the hit/miss reporting that was lost when
  `brigid_llm::DiskCache` was removed.
- Updated `BRIGID_NO_CACHE` help text to mention `brigid cache prune` and
  `brigid cache stats`.

### Phase 4: brigid-llm removal (issue #297)

- **Removed `brigid-llm` crate** from the workspace. The CLI now uses
  `llm_kernel::llm::{OpenAIClient, RetryClient, CacheClient}` directly — no
  adapter layer.
- The `LegacyLlmClient` adapter, `legacy_llm_error` mapping, and
  `brigid_llm::DiskCache` have been replaced with `llm_kernel` native types.
- **Provider resolution logic moved to `brigid_pipeline::llm`** as
  `resolve_llm_config` and `build_live_client`. This keeps the CLI a thin
  wrapper (per the ARCHITECTURE.md layering rule) and makes the security-
  critical provider/key-chain logic unit-testable in the library crate.
  15 new tests cover key isolation, blank-env handling, host allowlist
  enforcement, and provider inference from base URL.
- **Retry/backoff**: `OpenAIClient` is now wrapped in `llm_kernel::RetryClient`
  with the default `RetryConfig` (max 3 retries, 1s base delay, exponential
  backoff, Retry-After capped at 5 minutes). This matches the bounded
  exponential backoff guarantees of the removed `brigid-llm` client.
- **Cache backend changed** from `brigid_llm::DiskCache` (file-based with LRU
  eviction and size limits) to `llm_kernel::store::SqliteKvStore` (SQLite-backed
  KV store). Trade-offs:
  - The SQLite cache does **not** enforce a size limit. Use `brigid cache prune`
    to delete all cached responses, or set `BRIGID_NO_CACHE=1` to disable
    caching entirely.
  - Cache keys include the model name and full request JSON (via
    `llm_kernel::CacheClient`), so switching models or providers does not
    produce incorrect cache hits.
  - Cache hit/miss stats are available via `brigid_pipeline::StatsClient`,
    which probes the `KvStore` to track hits and misses. The CLI's verbose
    output reports cache status; use `brigid cache stats` for entry count
    and on-disk size.
  - The cache stores full prompt and response bodies on disk. Crawled
    repositories may contain secrets in source files; these are truncated and
    batched before being sent to the LLM, but the cache persists the full
    prompt+response. This is the same exposure profile as the previous
    `DiskCache`. Users with sensitive repos should disable caching
    (`BRIGID_NO_CACHE=1`) or point the cache at an encrypted volume.
- Removed `async-trait` dependency from `brigid-cli` (no longer needed without
  the adapter).

## [2.0.0] - 2026-08-17

Breaking release that adopts [`llm-kernel`](https://crates.io/crates/llm-kernel)
as the LLM provider layer and removes `brigid-llm` (issue #297, Phases 1–4).

### Changed

- **MSRV is now 1.92** (was 1.85). Required by `llm-kernel`. Rust 1.97+ is
  supported.
- **`brigid-pipeline` depends on `llm-kernel` 0.25** with features
  `client-async`, `provider`, `tokens`, `safety`, and `cache`. Stage code
  calls `LLMClient::complete(LLMRequest)` via `brigid_pipeline::llm::complete_text`
  instead of `brigid_llm::LlmClient::complete(&str)`.
- Workspace version **1.3.0 → 2.0.0**.
- The published crate version in the issue text (`0.19`) is superseded by the
  current docs.rs release (`0.25`), which provides `CacheClient` / `KvStore`.

### Removed

- **`brigid-llm` crate removed** (Phase 4, see [Unreleased] above). The 2.0.0
  release deprecated it; this release removes it entirely.

### Added

- `brigid_pipeline::llm` — `MockClient` implementing `llm_kernel::llm::LLMClient`,
  `complete_text`, and thin `bounded_complete` / `bounded_complete_with_budget`
  wrappers.

## [1.3.0] - 2026-08-07

Minor release adding two major features: an MCP (Model Context Protocol) server
for AI assistant integration (ADR 0015) and a Graph Provider abstraction for
structural code analysis integration (ADR 0016).

### Added

- **MCP server (ADR 0015).** New `brigid-mcp` crate exposes brigid's checkpoint
  knowledge graph to AI assistants (Claude Desktop, Cursor, Windsurf) via the
  Model Context Protocol. The server is read-only and loads a previously
  generated checkpoint directory into memory, serving:
  - **Resources** — `checkpoint://` URIs for metadata, abstractions,
    relationships, chapter ordering, file inventory, individual chapters, setup
    guide, architecture overview, and a combined tutorial index.
  - **Tools** — 7 graph query/lookup tools: `find_abstraction_for_file`,
    `abstraction_dependencies`, `files_for_abstraction`,
    `relevance_ranked_chapters`, `chapter_for_file`, `list_abstractions`,
    `is_checkpoint_stale`.
  - **Prompts** — 3 onboarding workflows: `onboard_to_codebase`,
    `explain_file`, `deep_dive_abstraction`.
  - **Transport** — stdio transport via the `rmcp` crate (v3.1.1), the official
    Rust MCP SDK. The server advertises all three capabilities (resources,
    tools, prompts) and supports cache hints for MCP 2026-07-28 clients.

- **Graph Provider abstraction (ADR 0016).** New `GraphProvider` trait in
  `brigid-core` allows external structural analysis tools (call graphs, code
  knowledge graphs) to enhance brigid's pipeline with structural data. The
  `NoneProvider` is the default — all methods return empty/`None`, so brigid
  works exactly as before (LLM-only) with zero configuration. The graph provider
  is an opt-in enhancement integrated into three pipeline stages:
  - **Identify stage** — community context in the map prompt, multimodal concept
    context in the reduce prompt.
  - **Relationships stage** — `structurally_verified: Option<bool>` field on
    `Relationship` records whether the graph provider's call graph confirms or
    contradicts each LLM-extracted edge.
  - **Order stage** — hub concepts from the graph provider inform early chapter
    placement for key abstractions.

- **`GraphProviderConfig`** in `RunConfig` — configurable via `[graph_provider]`
  in `brigid.toml`, `BRIGID_GRAPH_PROVIDER` environment variable, and CLI.
  Supports `type = "none"` (default) and `type = "codegraph"` with a `data_path`
  pointing to a CodeGraph SQLite database.

### Changed

- **New `brigid-mcp` crate** added to the workspace. Does not affect existing
  crates — the MCP server is a separate binary that reads checkpoints produced
  by `brigid generate`.

## [1.2.0] - 2026-07-30

Minor release adding OpenRouter as a first-class LLM provider (ADR 0017) and
wiring `RunConfig.provider` / `RunConfig.model` into live client construction
for all presets.

### Added

- **OpenRouter as a first-class LLM provider (ADR 0017).** Set
  `provider = "openrouter"` and an explicit model (e.g. `openai/gpt-4o`) in
  `brigid.toml`, or `BRIGID_PROVIDER` / `BRIGID_MODEL`. Defaults to
  `https://openrouter.ai/api/v1`, allowlists `openrouter.ai`, accepts
  `OPENROUTER_API_KEY`, and sends OpenRouter attribution headers. `RunConfig.provider`
  and `RunConfig.model` now drive live client construction for all presets
  (DeepSeek, OpenAI, OpenRouter, custom).

### Changed

- **`RunConfig.provider` and `RunConfig.model` are now operational.** These
  fields were previously parsed from `brigid.toml` / CLI / env but ignored
  during client construction. They now select provider presets (base URL,
  API-key env var, host allowlist, attribution headers) for DeepSeek, OpenAI,
  OpenRouter, and custom providers. This is a behavior change that enables new
  capability rather than a regression; existing env-only configurations
  (`BRIGID_LLM_BASE_URL`, `BRIGID_LLM_API_KEY`) continue to work unchanged.
- **OpenAI and OpenRouter now require an explicit model.** DeepSeek retains
  `deepseek-chat` as a safe default; OpenAI and OpenRouter have no safe
  universal default and will error with a clear message if no model is set.

## [1.1.0] - 2026-07-29

Minor release with three new features (blog-post tutorial style, lenient
app validation, incremental chapter regeneration), four bug fixes, and a
Windows CI flake fix.

### Added

- **Blog-post tutorial style (`--tutorial-style`).** Tutorials can now be
  generated in two styles: `blog` (new default — shorter, simpler, more
  conversational) and `book` (the previous long-form style). The flag
  selects the chapter outline and architecture overview templates. The
  `TutorialStyle` enum is threaded through `RunConfig`, `GenerateConfig`,
  `ChaptersConfig`, and `OverviewInput` (#267, #270).
- **`--strict-app-validation` flag.** When set, the overview stage fails on
  unknown app paths in LLM output. By default (changed from previous
  behavior), unknown apps now produce a warning instead of an error,
  letting generation proceed with the valid apps (#265, #269).
- **Incremental chapter regeneration.** When `--since` is set, only
  chapters whose abstractions touch changed files are re-generated.
  Chapters for unchanged abstractions are reused from the checkpoint,
  saving LLM calls and time on incremental runs. Uses a `HashSet` for
  O(1) path lookups with cross-platform normalization (#272).

### Changed

- **Default tutorial style is now `blog`.** Users who want the previous
  long-form output should pass `--tutorial-style book`. The blog style
  produces shorter chapters with fewer diagrams, optimized for quick
  onboarding rather than comprehensive reference.
- **Overview app validation is lenient by default.** Previously, unknown
  app paths in the architecture overview caused a hard error. Now they
  produce a warning and generation continues. Pass `--strict-app-validation`
  to restore the old behavior.

### Fixed

- **Checkpoint collision when `--dir` differs.** The `source_dir` field is
  now included in the checkpoint's `config_hash` identity check, preventing
  silent checkpoint reuse when the same output directory is used for
  different source directories (#266, #268).
- **Unclosed LLM code fences.** The YAML/JSON extraction layer now
  tolerates unclosed ```` ``` ```` fences in LLM output and sends
  `max_tokens` to prevent truncation (#262, #263).
- **Relationship endpoint range-checking.** Follow-up tests for PR #259:
  relationship endpoint tests, mock fallback warning, and `FORCE_MOCK`
  documentation (#260, #261).
- **Windows CI flake in cache tests.** `temp_root()` now creates the
  temp directory synchronously, eliminating a race on Windows CI runners
  where 8.3 short names caused `NotFound` errors (#271).

## [1.0.2] - 2026-07-27

Patch release that hardens the CLI's LLM-client selection and the
relationships stage, adds a security policy, and closes a codecov patch
coverage gap.

### Changed

- **CLI now fails closed on missing API credentials.** `build_real_llm_client`
  returns `Result` instead of `Option`; when no `BRIGID_LLM_API_KEY` /
  `DEEPSEEK_API_KEY` is set and `BRIGID_FORCE_MOCK` is not enabled, the CLI
  exits with a clear LLM-configuration error instead of silently emitting
  placeholder mock output. Anyone who relied on the silent mock fallback
  must now set `BRIGID_FORCE_MOCK=1` explicitly.
- **`BRIGID_FORCE_MOCK` recognizes falsy values.** `0`, `false`, `no`, `off`
  (case-insensitive) and empty/whitespace now disable forced mock output;
  any other non-blank value enables it. `None` (env var unset) is disabled.

### Added

- **`SECURITY.md`** policy with supported-versions statement and private
  vulnerability reporting instructions via GitHub Security Advisories.
- **Relationship endpoint range-checking.** The relationships stage now
  validates that `from_abstraction` / `to_abstraction` indices fall within
  the identify result, returning `RelationshipsError::EndpointOutOfRange`
  instead of panicking on malformed LLM output.
- **Centralized mock placeholder responses.** The `PLACEHOLDER_*` constants
  and `mock_client` helper are now shared across `cmd_identify`,
  `cmd_generate`, and `cmd_generate_each_app`, removing three-way
  duplication.
- **Mock fault-injection unit tests.** `mock_fail_error()` is extracted as a
  pure function and unit-tested for all 5 fault keywords (`timeout`,
  `ratelimit`, `provider`, `parse`, `network`/unknown).
- **Integration tests** for the non-`--single-shot` mock path,
  `--each-app --review-chapters`, and `--each-app --force-setup` branches.

### Fixed

- Codecov patch coverage on PR #259: 84.61% → 89.11% (target: 85%).

## [1.0.1] - 2026-07-27

Patch release to add per-crate README files and make the crates.io pages
self-documenting.

### Added

- README.md for each workspace crate (`brigid-core`, `brigid-crawl`, `brigid-llm`, `brigid-pipeline`, `brigid-cli`) with badges, descriptions, usage examples, and links back to the main [`brigid`](https://github.com/igmarin/brigid) repository.
- `readme = "README.md"` entry in each crate `Cargo.toml` so crates.io renders the README.

### Fixed

- Replaced the unsupported crates.io category slug `documentation` with `text-processing` in the workspace `Cargo.toml`.
- Moved prompt templates into `crates/brigid-pipeline/prompts` so they are included in the published `brigid-pipeline` crate.
- Made the release workflow's crates.io publish step idempotent, allowing re-runs to skip already-published workspace crates.

## [1.0.0] - 2026-07-27

First stable release. `brigid` is a Rust CLI that crawls a codebase,
identifies its core abstractions via LLM map/reduce, and produces a
multi-chapter Markdown + Mermaid tutorial explaining how the system works.
Built for monorepos and large codebases where "read the source" is not a
realistic onboarding path.

This release consolidates Milestones 0–6 (the full Python-to-Rust migration
and Phase 5 foundation) plus the v1.0.0 documentation refactor and two
proposed ADRs for the post-1.0 roadmap (MCP server, graph provider
integration).

### Added — v1.0.0 release

- **Lean, user-focused README** — rewritten from 686 to 184 lines with
  Mermaid pipeline and output-structure diagrams, quick start, CLI table,
  and links to deep documentation (#252).
- **`docs/usage-guide.md`** — deep command reference: every command, flag,
  environment variable, provider setup (DeepSeek, OpenAI, Ollama, LM
  Studio), performance tips, shell completions install, man page, examples
  (#252).
- **`docs/troubleshooting.md`** — exit codes, checkpoint recovery, LLM/
  cache/budget issues, corruption, `--since` requires git (#252).
- **`docs/project-status.md`** — milestones table (M0–M6), what works today,
  roadmap including proposed MCP server and graph provider integration
  (#252, #253, #255).
- **ADR 0015** — MCP server for codebase knowledge querying (proposed,
  post-v1.0.0). Exposes the checkpoint knowledge graph as MCP resources/
  tools/prompts so AI assistants can query on demand (#253).
- **ADR 0016** — Graph provider trait for structural ground truth from
  codegraph/Graphify (proposed, post-v1.0.0). Optional `GraphProvider`
  trait to improve abstraction identification and relationship verification
  on large codebases (#255).
- **Workspace layout** moved into `ARCHITECTURE.md` next to the crate
  hierarchy diagram (#252).
- **AI code review automation** and release/nightly CI documentation moved
  into `CONTRIBUTING.md` (#252).

### Consolidated features (M0–M6)

The following capabilities were delivered across Milestones 0–6 and are
all included in this first stable release:

**Pipeline (M0–M4):**
- Full `brigid generate` pipeline: crawl → identify → relationships →
  order → chapters → setup → overview → combine
- Map/reduce and single-shot LLM identify with bounded concurrency
- Checkpoint + resume (content-addressed, file-based stage outputs with
  SHA-256 verification)
- i18n chrome (English + Spanish)
- `--each-app` monorepo fan-out, `--review-chapters` second LLM pass
- Score-triggered setup guide, multi-app architecture overview

**Product polish (M5):**
- Homebrew (source build), `cargo install`, `cargo-binstall`, GitHub
  Releases (Linux x86_64 binary; macOS/Windows compile from source)
- Shell completions (bash, zsh, fish, PowerShell) and man page
- Disk cache with LRU eviction (enabled by default)
- `--concurrency` flag, criterion benchmarks, `brigid init` wizard
- Windows CI, Python deprecation + migration guide

**Phase 5 foundation (M6):**
- `--format json` on every stage with versioned `StageOutput<T>` envelope
  (ADR 0012)
- `--since <git-ref>` git-diff incremental crawl (ADR 0013)
- Plugin trait and registry for custom kind detectors (ADR 0014)
- Criterion benchmarks for all critical paths
- Audit-driven hardening of error handling, lock contention, test coverage

### Changed — v1.0.0 release

- **README.md** rewritten: 686 → 184 lines, user-focused with Mermaid
  diagrams, deep content moved to dedicated docs files (#252).
- **ARCHITECTURE.md** updated: workspace layout tree added, ADR table
  extended to 0016, related-documentation section updated (#252, #253,
  #255).
- **CONTRIBUTING.md** updated: AI code review automation section expanded
  with CI/release/nightly workflow details, documentation section updated
  (#252).
- **Version** bumped from 0.6.0 to 1.0.0 — first stable release.
- **Crates.io metadata** added to all workspace crates (keywords,
  categories) for publishing readiness.
- **Path dependencies** now specify `version.workspace = true` for
  crates.io publishing compatibility.
- **Release workflow** (`.github/workflows/release.yml`) now includes a
  `publish-crates` job to publish all workspace crates to crates.io in
  dependency order on tag push.
- **CI workflow** (`.github/workflows/ci.yml`) now includes:
  - **Codecov upload** — coverage reports (`lcov.info`) are uploaded to
    Codecov for trend tracking, PR comments, and badge integration
    (requires `CODECOV_TOKEN` secret).
  - **actionlint** — validates all GitHub Actions workflow files for
    syntax errors and unsupported features.
  - **Benchmarks** — criterion benchmarks run on main pushes
    (`cargo bench --workspace -- --quick`).
- **`codecov.yml`** — Codecov configuration with 85% project target
  (matches the CI hard gate), 1% threshold for trend tolerance, and
  fixture/benchmark/target directories excluded from reporting.
- **README badges** — CI, Codecov, crates.io, and license badges added.

### ADRs in this release

| ADR | Title | Status |
|-----|-------|--------|
| 0001 | Content-addressed checkpoint format for resume | Accepted |
| 0002 | `async-trait` for the `LlmClient` trait | Accepted |
| 0003 | `tokio::sync::Semaphore` for bounded map batches | Accepted |
| 0004 | Migration off unsound `serde_yml`/`libyml` | Superseded by 0005 |
| 0005 | Migration to `serde_yaml_ng` fork | Accepted |
| 0006 | File-based stage output storage with SHA-256 verification | Accepted |
| 0007 | Generic localization framework for tutorial chrome | Accepted |
| 0008 | Two-tier golden fixture strategy for eval regression | Accepted |
| 0009 | Disk cache enabled by default with LRU eviction | Accepted |
| 0010 | Release strategy (GitHub Releases, Homebrew, cargo) | Accepted |
| 0011 | Python deprecation: migration guide over wrapper | Accepted |
| 0012 | JSON output schema (`StageOutput<T>` envelope) | Accepted |
| 0013 | Git-diff incremental file detection (`--since`) | Accepted |
| 0014 | Plugin trait and registry for custom kind detectors | Accepted |
| 0015 | MCP server for codebase knowledge querying | Proposed |
| 0016 | Graph provider trait for structural ground truth | Proposed |

### Quality gates

- `cargo fmt --all -- --check` — pass
- `cargo clippy --workspace --all-targets -- -D warnings` — pass
- `cargo test --workspace` — 1,157+ tests pass
- `cargo llvm-cov --workspace --fail-under-lines 85` — 95.98% coverage
- 3-OS CI matrix (Ubuntu, macOS, Windows) — all green
- `cargo audit` + `cargo deny` — pass
- Eval regression gate + fixture baseline check — pass

## [0.6.0] - 2026-07-26

Milestone 6 — Phase 5 Foundation + Audit Hardening. The `brigid` CLI gains
machine-readable JSON output for all stages, git-diff incremental tutorials,
a plugin foundation for custom kind detectors, criterion benchmarks for all
critical paths, and audit-driven hardening of error handling, lock
contention, and test coverage.

### Added

- Typed JSON output schemas (`StageOutput<T>`) for all pipeline stages in
  `brigid-core::stage_output` with schema stability tests and ADR 0012
  (#220, #221, #222, #223).
- `--format json` flag on every stage subcommand (identify, relationships,
  order, chapters, setup, overview, combine) and `brigid generate`, emitting
  a `StageOutput<T>` envelope with `schema_version`, `stage`, `status`,
  `data`, and optional `stats` (#221, #222, #223).
- Per-stage LLM call tracking in `ProgressTracker` and `StageTiming`
  (#223).
- `git_diff` module in `brigid-crawl` for detecting changed files since a
  git ref via `git diff --name-only` (no libgit2 dependency), with
  `CrawlOptions { since }` for filtered crawl (#224, ADR 0013).
- `--since <ref>` CLI flag and `RunConfig.since` field (config precedence:
  CLI > file > env > defaults; `BRIGID_SINCE` env var) on `generate`,
  `dry-run`, `identify`, and `crawl` (#225).
- Git revision tracking in `CheckpointV1` (`git_commit`, `since_ref`) with
  staleness detection for incremental resume (#226).
- Incremental identify: `brigid generate --since <ref>` re-analyzes only
  modules with changed files, merges with checkpoint abstractions, and
  re-ranks; falls back to full identify when no checkpoint or stale
  (#227).
- Plugin trait and registry (`KindDetector`, `PluginRegistry`,
  `DefaultKindDetector`) in `brigid-core::plugin` for custom abstraction
  kind detectors; `RunConfig.plugin_dirs` configurable via `brigid.toml`
  `[plugins] dirs = [...]` and `BRIGID_PLUGIN_DIRS` env var (#228, ADR 0014).
- Criterion benchmarks for crawl walk, batch file packing, cache get/put,
  content redaction, YAML extraction, and module key computation (#229).
- ADRs 0012–0014: JSON output schema, git-diff incremental approach,
  plugin architecture (#223, #224, #228).
- Documentation: commit message conventions, testing strategy, getting
  help (CONTRIBUTING.md); troubleshooting, performance tips, Phase 5
  status (README.md); design principles, module table updates (ARCHITECTURE.md)
  (#247).
- Coverage tests for audit-identified gaps: checkpoint_store I/O faults,
  cache LRU eviction, generate cancellation, identify map+reduce
  orchestration (#230).

### Changed

- Hardened `DiskCache` mutex handling, URL host validation bypass, and
  bounded completion overflow handling (#212).
- Reduced lock contention in `review` stage with `AtomicBool` and
  channel-based chapter collection (#213).
- Reduced lock contention in `chapters` stage with clone-before-async and
  channel-based collection (#215).
- Fixed O(n) file context search in chapters with HashMap lookup (#216).
- Reduced string allocations in chapter writing hot path (#217).
- Replaced synchronous `std::fs` with `tokio::fs` in `DiskCache` for
  async cache I/O (#218).
- Single-pass string replacement for prompt sanitization and mermaid
  allocation reduction (#219).

## [0.5.0] - 2026-07-25

Milestone 5 — Product Polish. The `brigid` CLI is now a distributable product:
native installers, shell completions, a man page, disk cache by default,
concurrency flags, benchmarks, an init wizard, Windows CI, and Python
entrypoint deprecation.

### Added

- Symlink cycle detection in `brigid-crawl` to prevent infinite recursion
  (#196).
- Configurable LLM host allowlist via `BRIGID_LLM_ALLOWED_HOSTS` env var and
  `[[allowed_hosts]]` table in `brigid.toml` (#194).
- Disk cache enabled by default with LRU eviction and size limits (100 MB
  default); bypass with `BRIGID_NO_CACHE=1` (#197, ADR 0009).
- `CHANGELOG.md` and `ARCHITECTURE.md` documentation (#193).
- Batch file writes and dev profile optimizations in the pipeline (#200).
- Reduced lock contention in chapter summary collection (#199).
- Reduced excessive cloning in pipeline hot paths (#195).
- Concurrent checkpoint and filesystem edge case tests (#202).
- Concurrency (`--concurrency`), budget (`--max-llm-calls`), verbose
  (`--verbose` / `-v`), and quiet (`--quiet` / `-q`) CLI flags (#201).
- Criterion benchmarks for seven critical pipeline paths: template rendering,
  file context selection, checkpoint roundtrip, budget estimation, chapter
  generation, combine index, mermaid sanitization (#205).
- `brigid init` wizard with `--check` validation for starter `brigid.toml`
  (#203).
- CLI error path tests with `assert_cmd` improving `main.rs` coverage (#204).
- Shell completions for bash, zsh, fish, and PowerShell via `clap_complete`
  (#206).
- Man page generation via `clap_mangen` — `brigid manpage` produces a
  troff-formatted man page covering all subcommands (#207).
- Release workflow with native installers for Linux (x86_64, aarch64), macOS
  (x86_64, aarch64), and Windows (x86_64), plus a Homebrew formula template
  (#209, ADR 0010).
- Windows and macOS added to the CI test and clippy matrix (#208).
- Python entrypoint deprecation guide
  ([`docs/migrating-from-python.md`](docs/migrating-from-python.md)) with
  command mapping, environment variable changes, feature parity table, and FAQ
  (#191, ADR 0011).
- ADRs 0009–0011: disk cache default + LRU eviction, release strategy, Python
  deprecation approach (#192).

### Changed

- Documentation updated for M5 completion: README, CONTRIBUTING, ADRs,
  milestone table, tech stack (#192).
- [`docs/move-to-rust.md`](docs/move-to-rust.md) Phase 4 marked complete;
  Phase 5 clearly marked as optional/advanced (#192).

### Deprecated

- Python entrypoint (`pip install decon`) — deprecated in favor of the Rust
  CLI. No new features will be added to the Python implementation. See
  [`docs/migrating-from-python.md`](docs/migrating-from-python.md) (#191).

## [0.4.0] - 2026-07-25

Milestone 4 — Full Generate Path. The complete `brigid generate` pipeline is
working end-to-end, with i18n chrome, per-stage subcommands, monorepo fan-out,
chapter review, frozen fixtures, and a live smoke test.

### Added

- `brigid generate --dir PATH [--output-dir PATH] [--language en|es]
  [--each-app] [--review-chapters]` subcommand orchestrating the full pipeline:
  identify -> relationships -> order -> chapters -> setup -> overview ->
  combine (#146, #166).
- Per-stage subcommands for debugging individual pipeline stages: `brigid
  relationships`, `brigid order`, `brigid chapters`, `brigid setup`, `brigid
  overview`, `brigid combine` (#167).
- `--each-app` flag for per-app tutorial generation in monorepos (#168).
- `--review-chapters` flag for optional chapter quality polishing via a second
  LLM pass per chapter (#150, #170).
- i18n chrome system with English and Spanish locales (`Locale`,
  `ChromeStrings`) localizing index/footer headings and labels (#155, ADR 0007).
- Chapter domain types: `Chapter`, `ChapterOrder`, `ChapterResult` (#156).
- M4 domain types: `RelationshipsResult`, `Relationship` (#154);
  `SetupGuide`, `ArchitectureOverview`, `CombinedTutorial` (#157).
- `AnalyzeRelationships` pipeline stage with budgeted evidence selection
  (#162).
- `OrderChapters` pipeline stage with validation (#163).
- `WriteChapters` pipeline stage with bounded concurrency (#164).
- `WriteSetupGuide` pipeline stage with score-triggered generation (#160).
- `WriteArchitectureOverview` pipeline stage with app name validation (#161).
- `CombineTutorial` stage with deterministic diagrams and i18n chrome (#165).
- File-based checkpoint output storage for M4 stages with SHA-256 verification
  (#159, ADR 0006).
- Enriched golden tutorial fixtures and eval regression CI gate (#158).
- Eval regression CI gate with positive (`good-mini`) and negative
  (`broken-mini`) fixture tests (#169, ADR 0008).
- LLM-generated frozen fixture and nightly CI verification job (#171).
- Full-pipeline live LLM smoke test for monorepo generate (#172).

### Changed

- Documentation updated for M4 completion: README, CONTRIBUTING, ADRs,
  milestone table (#173).

## [0.3.0] - 2026-07-24

Milestone 3 — LLM Identify. The `LlmClient` trait and provider clients land,
map/reduce identify works with checkpoint resume and Ctrl+C graceful shutdown,
plus supply-chain hardening and refactoring.

### Added

- `Abstraction` and `Relationship` domain types as the M3 foundation (#81).
- `LlmClient` trait, `LlmError` enum, and `MockClient` test double (#63, #84).
- Robust YAML/JSON block extraction from messy LLM output (#64, #86).
- Prompt template rendering with `minijinja` plus snapshot tests (#65, #88).
- OpenAI-compatible provider client (`OpenAiCompatibleClient`) with
  retry/backoff/timeout and disk cache keyed by hash(prompt)+model+provider
  (#66, #89).
- Bounded concurrency for map batches via `tokio::sync::Semaphore` (#67, #91,
  ADR 0003).
- Identify single-shot stage for small repos (#69, #90).
- Identify map stage with batched, bounded-concurrent LLM calls (#70, #93).
- Identify reduce stage merging and ranking candidates (#71, #94).
- Checkpoint-after-identify and resume mid-identify matrix (#72, #96).
- Ctrl+C graceful shutdown with clean checkpoint dump (exit 5) (#68, #97).
- Opt-in live LLM smoke harness, budget-capped and feature-gated (#74, #92).
- Config-file secret-field guard rejecting `api_key`/`token` in `brigid.toml`
  (#73, #85).
- `cargo deny` (advisories + licenses + bans) added to CI alongside
  `cargo audit` (#76, #82).
- Disk response cache structure for LLM calls (no live calls) (#45).
- Progress tracker and max-LLM-calls budget (`ProgressTracker`) (#46).
- Secrets path classification and content redaction (#47).
- ADRs 0002-0004: async-trait LlmClient, bounded concurrency, YAML migration
  (#106, #120).

### Changed

- Folded file sizes into `crawl_local`, eliminating dry-run re-stat (#77, #83).
- Migrated `brigid-core` off unsound `serde_yml`/`libyml`
  (RUSTSEC-2025-0067/0068) to `serde_yaml_ng` (#75, #87, ADR 0005).
- Tracked `serde_yaml` 0.9 deprecation and migrated to `serde_yaml_ng` (#114,
  #125).
- Split `identify.rs` into a module tree (#105, #126).
- Replaced stringly-typed `IdentifyError` variants with typed `#[from]` errors
  (#104, #124).
- Post-M3 documentation audit fixing stale references and adding missing ADRs
  (#130).

### Fixed

- Propagated `reqwest` builder errors instead of silent fallback (#118).
- Validated LLM provider host before sending the Authorization header (#117).
- Treated blank env vars as unset in `from_env` (#116).
- Serialized env-mutating tests and hardened temp-dir uniqueness (#115).
- Added missing exit codes 3 (budget) and 4 (LLM) (#103, #122).
- Cleaned up `usize::try_from` overflow handling in `batch_files_by_size`
  (#113, #128).
- Eliminated unnecessary file-vector cloning in `cmd_identify` and checkpoint
  load (#111, #127).
- Applied `redact_content` to README and eval file reads (defense-in-depth)
  (#112, #129).

### Security

- Config-file secret-field guard prevents API keys from entering `brigid.toml`
  (#73).
- Provider host validation before sending credentials (#117).
- Secrets redaction applied to README and eval file reads (#112).

## [0.2.0] - 2026-07-23

Milestone 2 — Checkpoint, Config & Coverage. Content-addressed checkpoint
storage, `brigid.toml` configuration, and the 85% coverage hard gate.

### Added

- Checkpoint schema v1 types per ADR 0001 (#42).
- Checkpoint store save/load implementing ADR 0001 (#43).
- Resume stage-skip and partial regenerate matrix (#44).
- `RunConfig` with CLI > file > env > defaults layering (#41).
- `brigid init`, `brigid resume`, and config file/env wiring (#48).
- Workspace `llvm-cov` baseline documentation for the coverage gate (#40).
- CI enforcement of workspace `llvm-cov` >= 85% line coverage (#49).
- ADR 0001 for the content-addressed checkpoint schema (#4, #13).

### Changed

- M2 closeout documentation: README status and CONTRIBUTING (#50, #61).

## [0.1.0] - 2026-07-23

Milestone 1 — Crawl + Dry-run + Eval. The zero-LLM foundation: local crawl,
dry-run plan matching the frozen `baseline.json`, Mermaid sanitize, setup
assessment, and the structural eval port. Includes the M0 spec-freeze work
(workspace, CI, prompts, fixtures, CONTRIBUTING).

### Added

- Cargo workspace layout with five crates: `brigid-core`, `brigid-crawl`,
  `brigid-llm`, `brigid-pipeline`, `brigid-cli`.
- CI pipeline skeleton: fmt, clippy (`-D warnings`), test, `llvm-cov`, doc,
  `cargo audit` (#8, #9).
- `CONTRIBUTING.md` skeleton with TDD workflow and coverage gate (#3, #12).
- Prompt catalog: 10 versioned Jinja2 templates extracted from the Python
  reference with render tests.
- M1 parity fixtures (`python-lib`, `umbrella`, `js-lib`) and frozen
  `baseline.json` with a pure-Rust regenerator (#17).
- ADR 0001 checkpoint schema revisions (#4, #14).
- `brigid-core` module key and inventory discovery (#29).
- Monorepo scope filter with shared root scaffolding (#30).
- Setup assessment scoring from five README signals (#31).
- Context budget estimates for dry-run (#32).
- `brigid-crawl` local filesystem inventory with fixture parity (#33).
- Dry-run plan assembly with baseline parity (#34).
- Mermaid sanitize and light validate (#24).
- Deterministic index diagram builders (#25).
- Structural tutorial eval with golden fixtures (#26).
- `brigid crawl`, `brigid dry-run`, and `brigid eval` CLI subcommands (#27).
- M1 closeout documentation: README status, CONTRIBUTING, coverage summary
  (#28, #39).

### Fixed

- Mapped path/load errors to exit code 2 (config) in the CLI (#27).

[Unreleased]: https://github.com/igmarin/brigid/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/igmarin/brigid/releases/tag/v0.5.0
[0.4.0]: https://github.com/igmarin/brigid/releases/tag/v0.4.0
[0.3.0]: https://github.com/igmarin/brigid/releases/tag/v0.3.0
[0.2.0]: https://github.com/igmarin/brigid/releases/tag/v0.2.0
[0.1.0]: https://github.com/igmarin/brigid/releases/tag/v0.1.0
