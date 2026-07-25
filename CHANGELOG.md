# Changelog

All notable changes to `decon` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are written retroactively from the merged pull requests and milestone
descriptions in [`docs/move-to-rust.md`](docs/move-to-rust.md). Each milestone
corresponds to a minor release; the workspace `version` field in `Cargo.toml`
tracks the latest.

## [Unreleased]

### Added

- Python entrypoint deprecation guide
  ([`docs/migrating-from-python.md`](docs/migrating-from-python.md)) with
  command mapping, environment variable changes, feature parity table, and FAQ
  (#191).

### Changed

- [`docs/move-to-rust.md`](docs/move-to-rust.md) Phase 4 updated to mark Python
  deprecation as complete (#191).
- [`README.md`](README.md) updated to mention Python deprecation and link to
  the migration guide (#191).

## [0.4.0] - 2026-07-25

Milestone 4 — Full Generate Path. The complete `decon generate` pipeline is
working end-to-end, with i18n chrome, per-stage subcommands, monorepo fan-out,
chapter review, frozen fixtures, and a live smoke test.

### Added

- `decon generate --dir PATH [--output-dir PATH] [--language en|es]
  [--each-app] [--review-chapters]` subcommand orchestrating the full pipeline:
  identify -> relationships -> order -> chapters -> setup -> overview ->
  combine (#146, #166).
- Per-stage subcommands for debugging individual pipeline stages: `decon
  relationships`, `decon order`, `decon chapters`, `decon setup`, `decon
  overview`, `decon combine` (#167).
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
- Config-file secret-field guard rejecting `api_key`/`token` in `decon.toml`
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
- Migrated `decon-core` off unsound `serde_yml`/`libyml`
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

- Config-file secret-field guard prevents API keys from entering `decon.toml`
  (#73).
- Provider host validation before sending credentials (#117).
- Secrets redaction applied to README and eval file reads (#112).

## [0.2.0] - 2026-07-23

Milestone 2 — Checkpoint, Config & Coverage. Content-addressed checkpoint
storage, `decon.toml` configuration, and the 85% coverage hard gate.

### Added

- Checkpoint schema v1 types per ADR 0001 (#42).
- Checkpoint store save/load implementing ADR 0001 (#43).
- Resume stage-skip and partial regenerate matrix (#44).
- `RunConfig` with CLI > file > env > defaults layering (#41).
- `decon init`, `decon resume`, and config file/env wiring (#48).
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

- Cargo workspace layout with five crates: `decon-core`, `decon-crawl`,
  `decon-llm`, `decon-pipeline`, `decon-cli`.
- CI pipeline skeleton: fmt, clippy (`-D warnings`), test, `llvm-cov`, doc,
  `cargo audit` (#8, #9).
- `CONTRIBUTING.md` skeleton with TDD workflow and coverage gate (#3, #12).
- Prompt catalog: 10 versioned Jinja2 templates extracted from the Python
  reference with render tests.
- M1 parity fixtures (`python-lib`, `umbrella`, `js-lib`) and frozen
  `baseline.json` with a pure-Rust regenerator (#17).
- ADR 0001 checkpoint schema revisions (#4, #14).
- `decon-core` module key and inventory discovery (#29).
- Monorepo scope filter with shared root scaffolding (#30).
- Setup assessment scoring from five README signals (#31).
- Context budget estimates for dry-run (#32).
- `decon-crawl` local filesystem inventory with fixture parity (#33).
- Dry-run plan assembly with baseline parity (#34).
- Mermaid sanitize and light validate (#24).
- Deterministic index diagram builders (#25).
- Structural tutorial eval with golden fixtures (#26).
- `decon crawl`, `decon dry-run`, and `decon eval` CLI subcommands (#27).
- M1 closeout documentation: README status, CONTRIBUTING, coverage summary
  (#28, #39).

### Fixed

- Mapped path/load errors to exit code 2 (config) in the CLI (#27).

[Unreleased]: https://github.com/igmarin/decon-rs/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/igmarin/decon-rs/releases/tag/v0.4.0
[0.3.0]: https://github.com/igmarin/decon-rs/releases/tag/v0.3.0
[0.2.0]: https://github.com/igmarin/decon-rs/releases/tag/v0.2.0
[0.1.0]: https://github.com/igmarin/decon-rs/releases/tag/v0.1.0
