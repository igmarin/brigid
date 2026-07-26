# ADR 0014: Plugin Trait and Registry for Custom Kind Detectors

## Status

Accepted

## Date

2026-07-31

## Context

The **identify** stage classifies crawled files into abstraction "kinds"
(`"module"`, `"class"`, `"function"`, `"config"`, `"documentation"`, …).
Today these kinds come exclusively from LLM output during the identify
map/reduce (or single-shot) call. Users with domain-specific codebases —
internal frameworks, proprietary file formats, non-standard extensions —
have no way to teach `decon` about their own kind vocabulary without
modifying the core pipeline or crafting elaborate LLM prompts.

Issue #228 asks for a **plugin extension point** so users can plug
domain-specific classification logic into the identify stage without
touching core code. The extension must be:

1. **Simple to implement** — a small trait a user can implement in a few
   lines.
2. **Non-invasive** — the core pipeline stays unchanged; plugins are
   registered at the orchestration boundary.
3. **Fallback-safe** — when no plugin matches, the built-in heuristic
   (file-extension classification) applies.
4. **Forward-compatible** — the trait and registry design must not block
   future isolation models (WASM, subprocess, dynamic shared libraries).

## Decision

### 1. Object-safe `KindDetector` trait in `decon-core`

A new module `decon-core::plugin` defines:

```rust
pub trait KindDetector: Send + Sync {
    fn detect_kind(&self, file_path: &str, content: &str)
        -> Option<AbstractionKind>;
    fn name(&self) -> &str;
}
```

The trait is **object-safe**: no generics, no `Self` in return position,
no generic-associated types. This lets the registry store
`Vec<Box<dyn KindDetector>>` and dispatch dynamically. The `Send + Sync`
supertrait allows the registry to be shared across async tasks (the
identify stage is async and concurrent).

`detect_kind` receives both the file path and its content so detectors
can use either extension-based or content-based heuristics. Returning
`None` means "this detector does not match — try the next one".

### 2. `PluginRegistry` with first-match dispatch

```rust
pub struct PluginRegistry {
    detectors: Vec<Box<dyn KindDetector>>,
}
```

`PluginRegistry::detect_kind(file_path, content)` iterates detectors in
**registration order** and returns the first `Some` result. This gives
users predictable priority control: register the most specific detector
first, the most general (default) last.

`PluginRegistry::with_default()` pre-populates the registry with the
built-in `DefaultKindDetector` as the fallback, so callers always get a
sensible answer when no custom plugin matches.

### 3. Built-in `DefaultKindDetector`

`DefaultKindDetector` wraps the file-extension heuristic that the
identify stage has always implicitly relied on (the LLM is prompted with
kinds like `"module"`, `"config"`, `"documentation"`). It maps common
extensions to kinds:

| Extension(s) | Kind |
|---|---|
| `.rs`, `.go`, `.py`, `.ts`, `.js`, `.java`, `.c`, `.cpp`, `.rb`, … | `source` |
| `.md`, `.rst`, `.txt`, `.adoc` | `documentation` |
| `.toml`, `.yaml`, `.yml`, `.json`, `.ini`, `.cfg`, `.lock` | `config` |
| `.sh`, `.bash`, `.zsh`, `.ps1`, `.bat` | `script` |

It also has a content-based fallback: if the extension is unknown but
the content contains `pub mod` / `mod` (Rust) declarations, it returns
`"module"`. Unknown extensions with no module declaration return `None`.

### 4. `RunConfig.plugin_dirs`

`RunConfig` gains a `plugin_dirs: Option<Vec<PathBuf>>` field,
configurable via `decon.toml`:

```toml
[plugins]
dirs = ["./plugins", "./custom-detectors"]
```

The nested `[plugins]` table is lifted into the flat `plugin_dirs` field
during config parsing (TOML and YAML). The `DECON_PLUGIN_DIRS`
environment variable (colon-separated) is also supported.

**Dynamic loading from shared libraries (`.so`/`.dylib`/`.dll`) is
explicitly out of scope for this issue.** The `plugin_dirs` field is
parsed and stored today so config round-trips are stable, but the
identify stage uses an in-process `PluginRegistry`. Dynamic loading is a
future milestone (see §Future extension points).

### 5. Identify stage integration

The identify stage enriches abstractions via
`decon_core::plugin::enrich_abstraction_kinds` (and the pipeline-level
wrapper `decon_pipeline::identify::enrich_identify_kinds`). The
enrichment is **gap-filling only**: abstractions whose `kind` is already
non-empty (the normal case — the LLM set it) are left untouched. Only
abstractions with an empty `kind` are classified by the registry.

`identify_with_cancellation` (the cancellation-aware identify runner)
accepts an `Option<&PluginRegistry>` and applies enrichment after each
completed identify result (single-shot, reduce, or map-only). The CLI
builds a `PluginRegistry::with_default()` and passes it in, so the
built-in heuristic is always available as a fallback.

### 6. Trait object storage over generics

We chose `dyn KindDetector` (trait objects) over a generic
`Registry<D: KindDetector>` for simplicity:

- **Pros:** Heterogeneous registries (multiple detector types in one
  registry); no monomorphisation bloat; stable ABI boundary for future
  dynamic loading.
- **Cons:** One virtual call per `detect_kind` invocation. This is
  negligible — enrichment runs once per abstraction, not per file byte.

## Alternatives Considered

### Option A — Closed enum of kinds with a registration map

Instead of a trait, maintain a `HashMap<Extension, AbstractionKind>` that
users extend.

- **Pros:** Even simpler; no trait objects.
- **Cons:** Cannot express content-based heuristics; cannot express
  multi-file or cross-file logic; no place for a detector `name()` for
  diagnostics; not forward-compatible to WASM/subprocess isolation.
- **Rejected:** Too rigid for domain-specific detectors that need
  content inspection.

### Option B — Generic registry `Registry<D: KindDetector>`

Use a generic parameter instead of `dyn`.

- **Pros:** Zero virtual-call overhead; static dispatch.
- **Cons:** All detectors must be the same type (or wrapped in an enum);
  monomorphisation spreads through every call site; blocks future
  heterogeneous/dynamic loading.
- **Rejected:** The virtual-call cost is negligible and the flexibility
  of heterogeneous trait objects is essential.

### Option C — WASM-based plugins from day one

Define the trait over a WASM ABI so plugins run in a sandbox.

- **Pros:** Memory safety isolation; language-agnostic plugins.
- **Cons:** Adds a WASM runtime dependency to `decon-core` (which is
  intentionally I/O-free and lightweight); significant complexity for
  an MVP; the trait shape would be constrained by the ABI.
- **Rejected:** Premature for this milestone. The object-safe trait is
  designed so a WASM-backed `KindDetector` adapter can be added later
  without changing the trait or registry API.

## Consequences

- **Positive:** Users can extend the identify stage with domain-specific
  kind classification without modifying core pipeline code.
- **Positive:** The built-in `DefaultKindDetector` ensures a sensible
  fallback, so existing behaviour is preserved when no plugins are
  registered.
- **Positive:** The object-safe trait is a stable ABI boundary for
  future dynamic loading (WASM, subprocess, shared libraries).
- **Positive:** `RunConfig.plugin_dirs` is parsed and stored today,
  making the config surface forward-compatible.
- **Negative:** Dynamic loading is not yet implemented —
  `plugin_dirs` is reserved but unused by the loader. Users must
  register detectors in-process today (e.g., via a future
  `--plugins` flag or library API). This is documented as future work.
- **Negative:** One virtual call per `detect_kind` — negligible cost,
  but a generic registry would avoid it. Accepted for the flexibility
  trade-off.

## Future Extension Points

1. **Dynamic shared-library loading** — A future milestone can add a
   loader that reads `plugin_dirs`, discovers `.so`/`.dylib`/`.dll`
   files, and registers `KindDetector` implementations exported via a
   C ABI or `abi_stable` crate. The `PluginRegistry::register` API and
   the `dyn KindDetector` storage already support this.

2. **WASM sandboxed plugins** — A `WasmKindDetector` adapter that
   implements `KindDetector` by invoking a WASM module would slot into
   the existing registry without API changes.

3. **Subprocess plugins** — A `SubprocessKindDetector` that pipes
   `file_path`/`content` to a child process and reads the kind back.

4. **Plugin discovery & manifest** — A `plugin.toml` manifest format in
   each plugin directory declaring the detector name, priority, and
   entry point.

5. **Enrichment beyond `kind`** — The same registry pattern can be
   extended to tier detection, app assignment, or entry-file selection
   by adding new traits (`TierDetector`, `AppDetector`, …) that share
   the `PluginRegistry` dispatch model.

## Related Documents

- [`crates/decon-core/src/plugin.rs`](../../crates/decon-core/src/plugin.rs) —
  the `KindDetector` trait, `PluginRegistry`, `DefaultKindDetector`, and
  `enrich_abstraction_kinds` implementation.
- [`crates/decon-core/src/config.rs`](../../crates/decon-core/src/config.rs) —
  `RunConfig.plugin_dirs` and the `[plugins] dirs` lifting logic.
- [`crates/decon-pipeline/src/identify/mod.rs`](../../crates/decon-pipeline/src/identify/mod.rs) —
  `enrich_identify_kinds` pipeline integration.
- [`crates/decon-pipeline/src/identify_runner.rs`](../../crates/decon-pipeline/src/identify_runner.rs) —
  `identify_with_cancellation` registry wiring.
- ADR 0001 — Checkpoint schema v1 (the pipeline that consumes the
  identify result).
- Issue #228 — Plugin trait + registry for custom kind detectors.
