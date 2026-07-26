# Project Status

Milestone history, current capabilities, and roadmap for `decon`.

---

## Milestones

| Milestone | Goal | Status |
|-----------|------|--------|
| **M0** — Spec Freeze | Workspace layout, CI, CONTRIBUTING, ADR 0001, prompt catalog, test fixtures, parity baseline | ✅ Done |
| **M1** — Crawl + Dry-run + Eval | `decon crawl` / dry-run matching `baseline.json`; mermaid sanitize; setup-assessment parity; `decon eval` port | ✅ Done |
| **M2** — Checkpoint, Config & Coverage | Content-addressed checkpoint (ADR 0001); `decon.toml`; ≥85% coverage gate | ✅ Done |
| **M3** — LLM Identify | `LlmClient` trait + provider clients; map/reduce identify; checkpoint resume; Ctrl+C graceful shutdown | ✅ Done |
| **M4** — Full Generate | Relationships → order → chapters → setup → overview → combine; Spanish chrome; `--each-app`; `--review-chapters`; eval regression gate | ✅ Done |
| **M5** — Product Polish | Installers, man page, shell completions, disk cache, concurrency flags, benchmarks, init wizard, Windows CI, Python deprecation | ✅ Done |
| **M6** — Phase 5 Foundation + Audit Hardening | git-diff incremental (`--since`, ADR 0013); JSON output for all stages (ADR 0012); plugin trait/registry (ADR 0014); audit hardening; performance optimizations | ✅ Done |

Release history with per-change details lives in
[`CHANGELOG.md`](../CHANGELOG.md).

---

## What works today

- **Full `decon generate` pipeline** — crawl → identify → relationships →
  order → chapters → setup → overview → combine, with i18n chrome
  (English + Spanish), `--each-app` monorepo fan-out, and
  `--review-chapters` polishing.
- **JSON structured output** — `--format json` on every pipeline stage with
  a versioned `StageOutput<T>` envelope (ADR 0012).
- **Git-diff incremental** — `--since <git-ref>` limits the crawl to changed
  files (ADR 0013).
- **Plugin foundation** — `KindDetector` trait + in-process `PluginRegistry`
  for custom abstraction kind detectors (ADR 0014).
- **Checkpoint + resume** — `checkpoint.json` + `files.ndjson.gz`, file-based
  stage output storage with SHA-256 verification (ADR 0006).
- **LLM provider client** — OpenAI-compatible HTTP with retry/backoff/timeout,
  host allowlist, disk cache with LRU eviction, bounded concurrency.
- **Distribution** — Homebrew, `cargo install`, `cargo-binstall`, GitHub
  Releases (Linux, macOS, Windows), shell completions, man page.
- **Quality gates** — fmt, clippy (`-D warnings`), 3-OS test matrix, ≥85%
  coverage, `cargo audit`, `cargo deny`, eval regression, fixture baseline.

---

## What does not work yet

The Phase 5 foundation landed in M6. Remaining advanced items (not on the
current roadmap):

- **Dynamic plugin loading** — the `KindDetector` trait and in-process
  `PluginRegistry` are in place (ADR 0014), but loading plugins from shared
  libraries (`.so`/`.dylib`/`.dll`) or WASM modules is future work.
- **Incremental tutorial regeneration** — `--since` limits the crawl to
  changed files today; re-using prior chapter content for unchanged modules
  during `generate` is a future enhancement.

The Python entrypoint has been deprecated — see
[`docs/migrating-from-python.md`](migrating-from-python.md) for the
migration guide.
