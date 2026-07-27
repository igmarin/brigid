---
layout: default
title: "Move to Rust"
nav_order: 4
---

# Moving this project to Rust (as a real CLI)

This document explains **how the system works today**, why it is a strong candidate for a **Rust CLI**, what a Rust rewrite should preserve, and what is easy to underestimate when leaving Python/Pocket Flow.

It is a design/migration note—not a commitment that Rust is mandatory. The product value is the **pipeline and quality rules**, not the current runtime.

---

## 1. Do I agree it should become a CLI?

**Yes—with nuance.**

### Why it fits a CLI extremely well

| Characteristic | Why CLI fits |
|----------------|--------------|
| Batch, offline-ish job | Crawl → many LLM calls → write files; no interactive UI required |
| Scriptable | `make tutorial`, CI, monorepo recipes, `--each-app` loops |
| Local filesystem + optional GitHub | Classic CLI domain (walk dirs, gitignore, emit `output/`) |
| Long-running with resume | Checkpoints, dry-run, progress, budgets are CLI ergonomics |
| Configuration surface | Flags + env + `.env` map cleanly to clap + config crates |
| Distribution | Single binary (`brew`, `cargo install`, GitHub releases) beats “clone repo + venv + pocketflow” |

### What “CLI” should mean here

Not only “replace `python main.py` with a binary,” but:

- **Product name** as a tool (e.g. `codebase-tutorial`, `codemap-tutor`, `pocket-tutor`)  
- **Stable UX**: subcommands, man-page-quality `--help`, exit codes  
- **Composable**: stdout for machine-readable dry-run JSON; files for tutorials  
- **Safe defaults** for large monorepos (dry-run first, budgets, checkpoints)

### Where I only *partially* agree

| Temptation | Reality |
|------------|---------|
| “Rust will make LLM calls faster” | Network + model latency dominate; Rust helps crawl/IO/parsing, not GPT latency |
| “Rewrite everything at once” | High risk; the hard IP is prompts + quality gates + monorepo heuristics |
| “CLI replaces the library” | Keep a **library crate** + thin CLI so others embed the pipeline in services later |

So: **yes, CLI-first product in Rust is a great direction**—if you treat the current Python tree as a **reference implementation** of a well-defined pipeline, not as throwaway scripts.

---

## 2. How this project should work (product model)

### 2.1 User promise

> Given a GitHub repo or local tree, produce a **beginner-friendly multi-chapter tutorial** (Markdown + Mermaid) that explains core abstractions, relationships, setup (when docs are weak), and monorepo structure—optionally in another language.

### 2.2 Mental model: staged pipeline with durable state

```text
                    ┌─────────────┐
                    │  dry-run    │  (optional: no LLM)
                    └──────┬──────┘
                           ▼
┌──────────┐   ┌──────────▼──────────┐   ┌─────────────┐
│  Fetch   │──▶│ Identify (map/reduce)│──▶│ Relationships│
│ + scope  │   │ + enrich tiers       │   │ + kinds      │
└──────────┘   └──────────┬──────────┘   └──────┬──────┘
     │                    │                      │
     │              checkpoint.json ◄────────────┤
     │                    │                      ▼
     │                    │               ┌─────────────┐
     │                    │               │ Order chapters│
     │                    │               └──────┬──────┘
     │                    │                      ▼
     │                    │               ┌─────────────┐
     │                    │               │ Write chapters│
     │                    │               │ + diagrams   │
     │                    │               │ + evidence   │
     │                    │               └──────┬──────┘
     │                    │                      ▼
     │                    │               ┌─────────────┐
     │                    │               │ Setup guide? │
     │                    │               │ Overview?    │
     │                    │               └──────┬──────┘
     │                    │                      ▼
     │                    │               ┌─────────────┐
     │                    │               │ Combine index│
     │                    │               │ + sanitize   │
     │                    │               └──────┬──────┘
     │                    │                      ▼
     │                    │               ┌─────────────┐
     └────────────────────┴──────────────▶│ eval output │
                                          └─────────────┘
```

Every expensive stage should be **idempotent + checkpointed** so a 23-batch monorepo run can resume after failure.

### 2.3 Core domain objects

| Object | Role |
|--------|------|
| `FileBlob` | `{ path, content }` after crawl/filter |
| `ModuleKey` | e.g. `apps/nexus_hub`, `_root` |
| `Abstraction` | name, description, file indices, tier S/M/L, kind, apps, entry_files |
| `Relationship` | from/to indices, label, kind |
| `Chapter` | ordered abstraction + markdown body |
| `SetupAssessment` | score, gaps, needs_guide |
| `RunConfig` | includes, excludes, apps, language, diagram level, budgets |
| `Checkpoint` | completed stages + serializable shared state |

### 2.4 Non-negotiable behaviors (port these, not the Python)

1. **Smart crawl** — include/exclude globs, size caps, gitignore-aware walk  
2. **Monorepo scope** — `--apps` / `--exclude-apps`, shared root scaffolding  
3. **Context budgets** — per-file truncate, per-batch char budget, path stubs  
4. **Map-reduce identify** — batch by module, merge candidates  
5. **Quality chapter contract** — fixed outline, diagram quotas, grounding rules  
6. **Mermaid safety** — sanitize/validate; deterministic index maps + fallbacks  
7. **Setup intelligence** — score README/config; generate setup when weak  
8. **Architecture overview** for multi-app systems  
9. **i18n chrome** — Spanish (etc.) for fixed index strings, not only LLM prose  
10. **Operability** — dry-run, resume, only-chapters, each-app, eval, call budget  

Details live in [best-practices.md](./best-practices.md) and the current Python modules under `utils/` + `nodes.py`.

---

## 3. Why leave Python / Pocket Flow?

### Honest pros of current stack

- Fast iteration on prompts  
- Pocket Flow is tiny and easy to read  
- Ecosystem for quick LLM SDKs  

### Real pain you already felt

| Pain | Example |
|------|---------|
| Fragile runtime env | Empty `LLM_PROVIDER` from Make overriding `.env` |
| Dependency soup | gemini / openai / pathspec / dotenv / pocketflow |
| Distribution | Users need pyenv, venv, correct Python |
| Performance of crawl | Acceptable, but Rust shines on large trees + parallel walk |
| Type safety around pipeline state | `shared` dict is a bag of keys; bugs are runtime |
| Packaging a “product” | Harder to ship as `brew install …` |

Rust addresses **shipping, correctness of the pipeline shell, and local tooling**—not magic LLM quality.

---

## 4. Proposed Rust product shape

### 4.1 Crate layout (recommended)

```text
brigid/                    # workspace
├── crates/
│   ├── brigid-core/            # pure domain: models, pipeline traits, mermaid, budget
│   ├── brigid-crawl/           # local + GitHub fetch
│   ├── brigid-llm/             # provider clients (OpenAI-compatible, Gemini, …)
│   ├── brigid-pipeline/        # stage orchestration, checkpoint, dry-run plan
│   │   └── prompts/            # versioned prompt templates (not buried in code)
│   └── brigid-cli/             # clap binary
├── tests/
│   ├── fixtures/            # tiny repos
│   └── golden/              # eval expectations
└── README.md
```

**CLI binary** depends on pipeline; **core** has no network where possible (easier testing).

### 4.2 CLI surface (subcommand style)

```text
brigid dry-run   --dir PATH [--apps a b] [-o output]
brigid generate  --dir PATH | --repo URL  [all quality flags]
brigid resume    --checkpoint PATH
brigid each-app  --dir MONOREPO
brigid eval      --out output/Project
brigid providers # list configured providers / models
```

Keep compatibility aliases or a migration guide from `make tutorial` / `python main.py`.

### 4.3 Config layers (precedence)

1. CLI flags  
2. `brigid.toml` / `.brigid.yaml` in project or cwd  
3. Environment (`BRIGID_LLM_PROVIDER`, keys, …)  
4. Defaults in code  

Avoid the Make “export empty string” class of bugs: **never set env keys to blank**.

### 4.4 Checkpoint format

Stable, versioned JSON (or JSON + sidecar for large file corpora):

```json
{
  "version": 2,
  "completed_stages": ["fetch", "identify"],
  "config": { "...": "..." },
  "files_manifest": "files.ndjson.gz",
  "abstractions": [ ... ],
  "relationships": { ... }
}
```

**Tip you might miss:** storing full file bodies inside one giant JSON checkpoint does not scale. Prefer:

- `manifest` + content-addressed blobs, or  
- re-crawl with same filters on resume (hash of config → skip if unchanged)

### 4.5 LLM layer

- Trait `LlmClient { async fn complete(&self, prompt: &str) -> Result<String> }`  
- Implementations: OpenAI-compatible (DeepSeek, Kimi, local vLLM), Gemini, Anthropic later  
- Shared: retries with backoff, timeout, token/char budget guard, disk cache keyed by **hash(prompt)+model+provider**  
- Structured output: prefer **JSON schema** where the provider supports it; fall back to fenced YAML/JSON parse with the robust extractor you already learned to need  

### 4.6 Async runtime

- `tokio` for concurrent **map** batches (with concurrency limit!)  
- Crawl can be parallelized carefully (IO bound)  
- Do **not** fire 23 map calls unbounded—respect provider rate limits

### 4.7 Prompt management

Move prompts out of string soup in source:

- `crates/brigid-pipeline/prompts/identify_map.md.j2` (or handlebars / minijinja)  
- Version prompts with the tool version  
- Snapshot tests: render prompt with fixture context → stable hash  

---

## 5. Stage-by-stage Rust mapping

| Current Python | Rust responsibility |
|----------------|---------------------|
| `crawl_local_files` / `crawl_github_files` | `brigid-crawl`: `ignore`/`walkdir` + globset; GitHub via REST + token |
| `monorepo_scope` | pure functions in `brigid-core` |
| `context_budget` | pure functions + property tests |
| `IdentifyAbstractions` | pipeline stage + map concurrent + reduce |
| `AnalyzeRelationships` | stage + budgeted snippet picker |
| `OrderChapters` | stage |
| `WriteChapters` | stage + diagram enforce + evidence footer |
| `WriteSetupGuide` | stage + setup assess pure logic |
| `WriteArchitectureOverview` | stage |
| `CombineTutorial` / `diagram_builder` / `mermaid_safe` / `i18n_chrome` | pure renderers + sanitize |
| `scripts/eval_tutorial.py` | `brigid eval` subcommand |
| `Makefile` | still useful; thin wrapper around binary |

Pocket Flow’s graph becomes an explicit `Pipeline` enum/state machine—**clearer** than a generic node framework for this linear workflow.

---

## 6. What you might be missing

### 6.1 Product / UX

| Miss | Why it matters |
|------|----------------|
| **Name + positioning** | “Tutorial-Codebase-Knowledge” is a research demo name; a CLI needs a short verb/noun |
| **First-run wizard** | `brigid init` writes `brigid.toml`, checks API keys, sample dry-run |
| **Exit codes** | 0 ok, 2 config, 3 budget, 4 LLM, 5 partial success with checkpoint |
| **Machine-readable dry-run** | `--format json` for CI/agents |
| **Telemetry opt-in** | anonymous stage timings (off by default) to improve defaults |
| **Output themes** | “strict engineering” vs “beginner friendly” tone presets |

### 6.2 Correctness / quality

| Miss | Why it matters |
|------|----------------|
| **Prompt regression suite** | Without fixtures + golden evals, Rust rewrite will silently degrade tutorials |
| **Deterministic seeds** where APIs allow | Reproducible docs for the same commit |
| **Repo fingerprint** | `git rev-parse HEAD` + dirty flag in index metadata |
| **Secret scanning** | Never echo `.env` contents into prompts (redact while crawling) |
| **License / privacy** | Shipping analysis of private code; clear “data leaves the machine to provider X” warning |
| **Hallucination eval** | Structural eval ≠ factual correctness; optional “file path must exist” asserts help |

### 6.3 Scale / monorepo

| Miss | Why it matters |
|------|----------------|
| **Large monorepos can need dozens of map batches** | Default should encourage `--apps` or auto-cap batches with a warning |
| **Adaptive batching** | If provider context is 128k vs 32k, batch size should depend on model profile |
| **Map concurrency + rate limits** | Rust makes it easy to DDoS your own API key |
| **Incremental by git diff** | “Only re-explain modules changed since tag” is huge for monorepos |
| **Language packs for crawl** | Elixir/Ruby includes were ad hoc; config tables per ecosystem |

### 6.4 Engineering the rewrite

| Miss | Why it matters |
|------|----------------|
| **Parity tests vs baseline** | Run `brigid crawl` on fixture repos; compare dry-run stats against `baseline.json` — not exact prose |
| **Feature flags** | Ship crawl+dry-run+eval first; LLM stages second |
| **Don’t rewrite Make away too early** | Keep Make calling the binary for human muscle memory |
| **Skipping TDD “to go faster”** | You will re-discover every monorepo edge case without tests |
| **Coverage theater** | 85% only counts if asserts are meaningful |
| **Docs after launch** | CLI adoption dies without install + resume + provider docs on day one |
| **Windows path semantics** | Monorepos on Windows; normalize `\` early |
| **Streaming write** | Write chapters as they complete so users see progress on disk |
| **Cancellation** | Ctrl+C saves checkpoint cleanly |

### 6.5 Business / distribution (if you care later)

- Binary signing / notarization (macOS)  
- Plugin model for custom “kind” detectors  
- Hosted mode later: same `brigid-core`, different front-end—CLI remains the reference client  

---

## 7. Suggested migration plan (incremental)

### Phase 0 — Spec freeze (1–2 days)

- Freeze stage names, checkpoint schema v1, CLI flag list  
- Extract prompts to files **in Python first** (makes porting trivial)  
- List fixtures: tiny Python lib, tiny Elixir umbrella, one JS package  
- Capture parity baseline from Python reference into `tests/fixtures/baseline.json`  
- Agree engineering bar: **TDD**, **≥ 85% coverage**, **documentation gates** (this section)  
- Add `CONTRIBUTING.md` skeleton before significant Rust code  

> **Baseline strategy (completed in M0).** The frozen `baseline.json`
> was originally produced by the Python reference's `crawl_local_files`,
> `monorepo_scope`, and `assess_setup_docs`. A pure-Rust regenerator
> (`tests/fixtures/regenerate_baseline.rs`, zero dependencies) now reproduces
> that output byte-for-byte, so no Python toolchain is needed to verify or
> regenerate the baseline. CI runs `regenerate_baseline --check` on every push
> to guard against accidental fixture drift. The regenerator is a **standalone
> reimplementation** of the reference heuristics — it is NOT `brigid-crawl`.
> When `brigid-crawl` is built in Phase 1, it is tested against the same frozen
> `baseline.json`, keeping the parity test non-circular.

### Phase 1 — Rust skeleton (no LLM)

- `brigid crawl` / dry-run plan equal to `baseline.json` on fixtures  
- mermaid sanitize + index builder parity  
- setup assessment pure logic parity (verified against `baseline.json` setup scores)  
- `brigid eval` port  

**Exit criteria:** dry-run stats match `baseline.json` exactly; eval works on existing `output/` samples; **≥ 85% coverage** on core/crawl; TDD used for budget/scope/mermaid; rustdoc on public API; CONTRIBUTING describes the test workflow.

### Phase 2 — LLM identify only (Milestone M3)

Tracked as GitHub milestone **M3 — LlmClient & Map-Reduce Identify**. Tickets:

- #62 Abstraction + Relationship domain types (foundation)
- #63 `LlmClient` trait + `MockClient` (tests-first, no network)
- #64 Robust YAML/JSON block extraction from messy LLM output
- #65 Prompt template rendering (minijinja) + snapshot tests
- #66 OpenAI-compatible provider client (reqwest, retry/backoff/timeout, cache)
- #67 Bounded concurrency for map batches (tokio semaphore)
- #68 Ctrl+C graceful shutdown → clean checkpoint dump (exit 5)
- #69 Identify single-shot stage (small repos)
- #70 Identify map stage (batched, bounded-concurrent)
- #71 Identify reduce stage (merge + rank → final list)
- #72 Checkpoint-after-identify + resume mid-identify matrix
- #73 Config-file secret-field guard (reject api_key/token in brigid.toml)
- #74 Opt-in live LLM smoke harness (budget-capped, feature-gated)

Tech-debt tickets that should land with M3 (supply-chain + perf):

- #75 Migrate off unsound `serde_yml`/`libyml` (RUSTSEC-2025-0067/0068)
- #76 Add `cargo deny` (advisories + licenses + bans) to CI
- #77 Fold file sizes into `crawl_local` (eliminate dry-run re-stat)

- Provider clients + cache + budget  
- Map-reduce identify + enrich  
- Checkpoint after identify  

**Exit criteria:** resume works; candidate counts sane on fixtures.

### Phase 3 — Full generate path (Milestone M4 — complete)

Tracked as GitHub milestone **M4 — Full Generate Path**. All 20 M4 issues
(#131–#152) are closed. The full `brigid generate` pipeline is working:

- `brigid generate` subcommand orchestrating: identify → relationships → order
  → chapters → setup → overview → combine
- Per-stage subcommands for debugging: `brigid relationships`, `brigid order`,
  `brigid chapters`, `brigid setup`, `brigid overview`, `brigid combine`
- `--each-app` flag for per-app tutorial generation in monorepos
- `--review-chapters` flag for optional chapter quality polishing
- i18n chrome (English + Spanish locales) via `ChromeStrings` trait
- File-based checkpoint output storage with SHA-256 verification (ADR 0006)
- Eval regression CI gate on golden fixtures (good-mini + broken-mini)
- LLM-generated frozen fixture + nightly CI verification
- Live full-pipeline smoke test (feature-gated)

**Exit criteria:** met — `brigid eval` score ≥ threshold on golden fixtures;
eval regression gate in CI; nightly LLM smoke verifies against frozen
`llm-generated` fixture.

### Phase 4 — Polish product (Milestone M5 — complete)

Tracked as GitHub milestone **M5 — Product Polish**. All M5 issues (#174–#191)
are closed. The product is now polished and distributable:

- **Native installers** — Homebrew formula, `cargo install`, `cargo-binstall`,
  and GitHub Releases with pre-built binaries for Linux (x86_64, aarch64),
  macOS (x86_64, aarch64), and Windows (x86_64) (ADR 0010).
  > **Update:** the initial release ships pre-built binaries for Linux x86_64
  > only. macOS and Windows users install from source via Homebrew or
  > `cargo install`. See ADR 0010 for the current release strategy.
- **Shell completions** — `brigid completions --shell bash|zsh|fish|powershell`
  via `clap_complete`.
- **Man page** — `brigid manpage` generates a troff-formatted man page via
  `clap_mangen`.
- **Disk cache by default** — LLM responses cached on disk with LRU eviction
  and size limits (ADR 0009). Bypass with `BRIGID_NO_CACHE=1`.
- **Concurrency / budget / verbose / quiet flags** — `--concurrency`,
  `--max-llm-calls`, `--verbose` / `-v`, `--quiet` / `-q`.
- **Symlink cycle detection** — crawl aborts on symlink loops instead of
  recursing infinitely.
- **Host allowlist** — configurable LLM provider host allowlist via
  `BRIGID_LLM_ALLOWED_HOSTS` or `[[allowed_hosts]]` in `brigid.toml`.
- **Criterion benchmarks** — seven benchmark suites for critical pipeline paths
  (template rendering, file context selection, checkpoint roundtrip, budget
  estimation, chapter generation, combine index, mermaid sanitization).
- **Init wizard** — `brigid init` with `--check` validation for starter
  `brigid.toml`.
- **CLI error path tests** — `assert_cmd` tests for exit codes and error
  messages, improving `main.rs` coverage.
- **Windows CI** — test and clippy jobs run on Ubuntu, macOS, and Windows.
- **Python deprecation** — the Python entrypoint is deprecated; a migration
  guide is published at
  [`docs/migrating-from-python.md`](./migrating-from-python.md) (ADR 0011).
  Since the Python code lives in a separate repository, Option B (migration
  guide, no wrapper) was implemented. The Rust CLI is now the canonical
  entrypoint — `pip install decon` users should switch to `brew install brigid`
  or `cargo install brigid-cli`.

**Exit criteria:** met — installers, completions, man page, cache-by-default,
Windows CI, and Python deprecation all landed.

### Phase 5 — Foundation landed in M6 (Milestone M6 — complete)

Tracked as GitHub milestone **M6 — Phase 5 Foundation + Audit Hardening**.
The three Phase 5 foundation items are now implemented, along with audit
hardening and performance optimizations:

- **git-diff incremental file detection** (`--since <git-ref>`) —
  `brigid-crawl::git_diff` shells out to `git diff --name-only` with
  triple-dot merge-base semantics and filters the crawl inventory to changed
  files. No libgit2 dependency. See
  [ADR 0013](./adr/0013-git-diff-incremental.md).
- **JSON structured outputs** — all pipeline stages (`identify`,
  `relationships`, `order`, `chapters`, `setup`, `overview`, `combine`,
  `generate`) now support `--format json` via a versioned `StageOutput<T>`
  envelope with frozen schema-stability snapshots. See
  [ADR 0012](./adr/0012-json-output-schema.md).
- **Plugin architecture** — an object-safe `KindDetector` trait and
  `PluginRegistry` with first-match dispatch let users plug domain-specific
  classification into the identify stage. The built-in
  `DefaultKindDetector` fills gaps when the LLM leaves a `kind` empty. See
  [ADR 0014](./adr/0014-plugin-architecture.md).

**Remaining advanced items (not on the current roadmap):**

- **Dynamic plugin loading** — loading `KindDetector` implementations from
  shared libraries (`.so`/`.dylib`/`.dll`) or WASM modules. The trait and
  registry are designed as a stable ABI boundary for this future work;
  `RunConfig.plugin_dirs` is already parsed and stored.
- **Incremental tutorial regeneration** — `--since` limits the crawl to
  changed files today; re-using prior chapter content for unchanged modules
  during `generate` is a future enhancement.

**Exit criteria:** met — git-diff incremental, JSON output for all stages,
and the plugin trait/registry all landed with ADRs and tests.

---

## 8. Engineering standards: best practices, TDD, coverage, documentation

The Rust rewrite is not only a port of behavior. It is a chance to **raise the quality bar**. Treat the following as **non-negotiable release criteria** for the CLI and library crates—not optional polish.

### 8.1 Best practices (engineering + product)

Align implementation with [best-practices.md](./best-practices.md) **and** general Rust/CLI hygiene:

| Area | Expectation |
|------|-------------|
| **Pipeline purity** | Core stages are testable functions/traits; CLI only parses args and prints |
| **Errors** | Typed errors in libraries (`thiserror`); user-facing messages + exit codes in CLI |
| **Config** | Never export blank env vars; clear precedence CLI > file > env > defaults |
| **Secrets** | Redact while crawling; never log API keys; document data leaving the machine |
| **Operability** | Dry-run, checkpoint, budgets, progress—same product rules as today |
| **Monorepo safety** | Default warnings for huge trees; encourage app scope |
| **Async discipline** | Bounded concurrency for map LLM calls; timeouts; cancellation saves checkpoint |
| **Semver** | Library crates versioned; prompt template changes may be breaking for golden tests |
| **Supply chain** | `cargo deny` / audited deps for HTTP and TLS stacks |

Product best practices (chapter contract, mermaid policy, setup scoring, evidence footers) stay **source of truth** in `docs/best-practices.md`; Rust must implement them, not reinvent ad hoc prompts.

### 8.2 TDD (test-driven development)

**Default workflow for every stage and pure helper:**

```text
1. Write a failing test that expresses the contract
2. Run it — confirm RED (fails for the right reason)
3. Implement the minimum code to pass
4. Run it — GREEN
5. Refactor with tests still green
6. Add property/edge cases only after the happy path is locked
```

#### What to TDD first (high ROI)

| Layer | Examples |
|-------|----------|
| **Pure domain** | module keys, app filters, setup score, tier inference, mermaid sanitize, diagram builders, i18n chrome |
| **Budgeting** | truncate, batch packing, path stubs, relationship file selection |
| **Parsing** | YAML/JSON extraction from messy LLM output |
| **Checkpoint** | save/load, stage skip, partial regenerate |
| **CLI** | clap parsing, exit codes, dry-run JSON shape (no network) |
| **Eval** | score fixtures under `tests/fixtures/tutorials/` |

#### What not to block on full TDD

- Live provider integration (use contract tests + recorded fixtures / wiremock)  
- Full “generate a real monorepo” E2E in unit CI (nightly smoke with a real key, optional)  

#### TDD rules of engagement

1. **No production code for a new behavior without a failing test first** (library code especially).  
2. **Bugfix = regression test first**, then fix.  
3. **Prompt changes** require at least: render snapshot + one parse/validate test of expected structure.  
4. **Refactor freely** only behind green suites.  
5. Prefer **table-driven tests** and **`insta` snapshots** for mermaid/index markdown.  

### 8.3 Test coverage target: **≥ 85%**

Coverage is a **floor**, not the goal—but it is a **gate**.

| Scope | Target |
|-------|--------|
| Workspace overall (line coverage) | **≥ 85%** |
| `brigid-core` (pure logic) | **≥ 90%** preferred |
| `brigid-cli` | lower OK if thin; critical paths still tested |
| LLM HTTP clients | mock-based; don’t chase 100% on generated clients |

#### How to enforce

- `cargo llvm-cov` (or `tarpaulin`) in CI  
- Fail PR if coverage **drops below 85%** or drops more than N% vs main without waiver  
- Exclude only: generated code, trivial `main` glue, vendor—document excludes in `codecov.yml` / CI config  
- Publish a coverage summary on each PR  

#### Coverage that matters (quality of tests)

| Prefer | Avoid |
|--------|--------|
| Branching in sanitize/budget/scope | Snapshotting entire LLM essays |
| Error paths (bad YAML, empty provider, budget exceeded) | Tests that only assert `is_ok()` |
| Checkpoint resume matrix | Duplicating the same happy path 20 times |
| Eval scoring edge cases | Hitting live APIs in unit tests |

**85% with weak asserts is a false green.** Pair coverage gate with:

- Mutation testing later (optional, `cargo-mutants` on `brigid-core`)  
- Eval score thresholds on fixture tutorials  
- Parity tests vs `baseline.json` on dry-run counts for frozen fixtures  

### 8.4 Documentation (required, not optional)

Documentation ships **with** the binary, not after.

#### User-facing

| Artifact | Content |
|----------|---------|
| **README** | Install, 60-second dry-run, first generate, providers, resume |
| **`--help` / man page** | Every subcommand and flag; examples |
| **Shell completions** | bash/zsh/fish via clap |
| **`docs/best-practices.md`** | Keep language-agnostic product rules updated |
| **`docs/move-to-rust.md`** | This migration contract |
| **Changelog** | User-visible changes per release |
| **Privacy note** | What is sent to which LLM provider |

#### Contributor-facing

| Artifact | Content |
|----------|---------|
| **ARCHITECTURE.md** (or `docs/architecture.md`) | Crates, stage diagram, data flow |
| **CONTRIBUTING.md** | TDD workflow, coverage gate, how to run tests |
| **Module-level rustdoc** | All public items in `brigid-core` / `brigid-pipeline` |
| **Prompt catalog** | What each prompt is for; input/output contract |
| **Fixture guide** | How to add a tiny repo fixture + expected dry-run stats (`tests/fixtures/README.md`) |

#### Documentation gates (CI)

1. `cargo doc --workspace --no-deps` succeeds  
2. **No missing rustdoc** on public API in library crates (`#![deny(missing_docs)]` or crate-level warn→deny)  
3. README examples stay valid (optional: `trycmd` / `assert_cmd` on `--help`)  
4. Link check on `docs/**/*.md` in CI (optional but valuable)  

#### Language

- User and contributor docs in **English** by default  
- Product still **generates** tutorials in Spanish/etc. via `--language`  
- If you maintain Spanish onboarding, keep it as a translated README section—not a substitute for English source docs  

### 8.5 Definition of Done (per feature / stage)

A stage (e.g. `Identify`, `Combine`) is not “done” until:

- [ ] Spec written (inputs/outputs/errors)  
- [ ] Failing tests written first (TDD)  
- [ ] Implementation green  
- [ ] Coverage for touched crates still **≥ 85%**  
- [ ] rustdoc on new public items  
- [ ] User-facing flag/docs updated if behavior is visible  
- [ ] Checkpoint/resume behavior covered if stage is durable  
- [ ] Best-practice product rules still hold (or doc updated deliberately)  

### 8.6 Suggested CI pipeline (Rust)

```text
fmt → clippy -D warnings → test → llvm-cov (≥85%) → doc →
  dry-run fixture parity → eval on golden tutorial fixtures →
  (nightly) optional live LLM smoke
```

### 8.7 Phasing quality work (don’t wait until the end)

| Phase | Quality bar |
|-------|-------------|
| Phase 1 (crawl/dry-run/eval) | TDD from day one; coverage ≥ 85% on `brigid-core` + crawl already |
| Phase 2 (identify) | Contract tests for map/reduce parse; mock LLM |
| Phase 3 (full generate) | Golden eval fixtures; docs for every subcommand; eval regression gate; nightly LLM smoke |
| Phase 4 (product polish) | man page, completions, benchmarks, Windows CI, Python deprecation |
| Phase 5 (foundation — M6) | git-diff incremental, JSON output schema stability tests, plugin trait/registry, audit hardening |

**Anti-pattern:** “We’ll add tests after the port works.” That recreates the Python repo’s fragility. The port **is** the moment to lock behavior with tests.

---

## 9. Technology recommendations (opinionated but flexible)

| Concern | Crate / approach |
|---------|------------------|
| CLI | `clap` derive |
| Shell completions | `clap_complete` (bash, zsh, fish, PowerShell) |
| Man page | `clap_mangen` (troff output) |
| Async | `tokio` |
| HTTP | `reqwest` |
| GitHub | REST via reqwest; optional `octocrab` |
| Walk + gitignore | `ignore` (ripgrep's walker) |
| Globs | `globset` / `wax` |
| Serialization | `serde` + `serde_json` |
| YAML prompts out | `serde_yaml_ng` for parsing model output |
| Templates | `minijinja` or `handlebars` |
| Errors | `thiserror` + `anyhow` in CLI |
| Tracing | `tracing` + `tracing-subscriber` |
| Config | `figment` or `config` |
| Tests | `insta` snapshots for prompts/mermaid |
| Benchmarks | `criterion` for critical pipeline paths |
| Release | `cargo-dist` / `goreleaser`-style GH Actions |

---

## 10. What *not* to over-optimize

- Reimplementing a general “agent framework” in Rust—your graph is linear  
- Perfect AST-based understanding of every language on day one—path heuristics + LLM is the product  
- Bit-identical chapter text vs Python—unachievable and pointless  
- Premature multi-agent debate loops—costly, weak ROI for onboarding docs  

---

## 11. Success metrics for the move

| Metric | Target |
|--------|--------|
| Install time for a new user | &lt; 2 minutes to first dry-run |
| Dry-run on 2k-file monorepo | seconds–low minutes, clear plan |
| Resume after kill mid-relationships | zero re-identify if checkpoint valid |
| Eval score on fixtures | no regression vs frozen eval snapshots (Phase 3 — complete) |
| Full generate UX | progress stages + call counts + checkpoint path always visible |
| “Works with empty PROVIDER make bug” class | impossible by design |
| Line coverage (CI) | **≥ 85%** workspace; fail under |
| TDD discipline | New core behavior lands with tests-first history (reviewable) |
| Public API docs | `cargo doc` clean; missing_docs denied on lib crates |
| CONTRIBUTING / ARCHITECTURE | Present before v0.1 public binary |

---

## 12. Bottom line

| Question | Answer |
|----------|--------|
| Is this a perfect CLI candidate? | **Yes**—batch analysis, files in/out, flags, CI, monorepo ops |
| Should it be Rust? | **Strong yes if you want a shippable tool**; not required for prompt R&amp;D |
| What’s the real asset? | Pipeline semantics + quality/mermaid/setup rules + operability |
| Biggest miss if you rush? | Checkpoint design for large file corpora, rate-limited map concurrency, prompt parity tests, secret redaction |
| Best first Rust milestone? | **Dry-run + crawl + eval + mermaid/index pure logic**—prove value without LLM spend |
| Engineering bar? | **TDD by default**, **≥ 85% coverage gate**, **docs (rustdoc + README + CONTRIBUTING) ship with features** |

---

## 13. Related docs

- [Best practices](./best-practices.md) — quality and monorepo principles (language-agnostic)  
- Root README — current Python CLI / Make UX to preserve in spirit  

---

## 14. Tech-debt register

Findings from the M2 validation review (2026-07-24). Each item is a tracked
GitHub issue so debt does not rot into "later means never".

| Issue | Severity | Milestone | Summary |
|-------|----------|-----------|---------|
| #75 | High (supply chain) | M3 (resolved) | `serde_yml` 0.0.12 + `libyml` 0.0.5 unsound/unmaintained (RUSTSEC-2025-0067/0068). Migrated to `serde_yaml_ng` (ADR 0005). |
| #76 | High (supply chain) | M3 (resolved) | Add `cargo deny` (advisories + licenses + bans) to CI. Now running on every PR. |
| #77 | Medium (perf) | M3 (resolved) | `dry_run` re-stats every file; folded sizes into `crawl_local`. |
| #73 | High (security) | M3 (resolved) | Config-file secret-field guard. Landed with M3. |
| #78 | Low (coverage) | M5 (resolved) | `brigid-cli/main.rs` at 64% line coverage; add assert_cmd error-path tests. Resolved by #204. |
| #79 | Low (UX) | M5 (resolved) | `load_file_config` extension detection + `cmd_resume` dir-exists check (rs-guard review Important #3/#4). |

### M2 review verdict (rs-guard, review-result.txt)

The M2 rs-guard review returned **NEGATIVE** with 2 "Security" findings.
Investigation during validation:

- **Security #1 (blank env var treated as set):** **False positive.**
  `config_from_env_map` uses `nonblank()` which trims and filters empty
  values; test `blank_env_does_not_override_defaults` covers it. The reviewer
  did not see the implementation in the diff.
- **Security #2 (config file accepting secrets):** **Not yet exploitable**
  (`RunConfig` has no secret fields) but a valid forward-looking concern.
  Tracked as #73 — a deny-list guard landing with M3.

No code change was required for M2 itself; the forward-looking guard is #73.

---

*When you start the rewrite, treat this file as the product spec for `brigid-pipeline` stages; update it when stage contracts change so Python and Rust do not diverge silently.*
