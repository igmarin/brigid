# ADR 0009: Disk Cache Default + LRU Eviction Strategy

## Status

Accepted

## Date

2026-07-25

## Context

The `decon-llm` disk cache (`DiskCache` in `decon-llm::cache`) stores LLM
responses keyed by `sha256(prompt + model + provider + extras)`. It was
introduced in M3 (#45) as an opt-in structure with no live calls. During M5
(#197), the cache was promoted to **enabled by default** with automatic LRU
eviction and a configurable size limit.

### Why cache by default?

- **Re-runs are free.** The most common `decon generate` workflow is iterative:
  a user runs the pipeline, inspects the output, tweaks a flag, and re-runs.
  Without caching, every re-run re-sends identical prompts to the provider,
  costing money and time for byte-identical responses.
- **Nightly CI benefits.** The nightly LLM smoke job (ADR 0008, Tier 2) relies
  on the cache to keep costs down when prompts have not changed between runs.
- **Checkpoint + cache are complementary.** Checkpointing (ADR 0001) skips
  entire stages; the cache skips individual LLM calls within a stage. A
  checkpoint resume still benefits from the cache for any partially completed
  stage.

### Constraints

- The cache must not grow unbounded — a long-running user could accumulate
  gigabytes of cached responses across many projects.
- Cache writes must not block the pipeline's critical path noticeably.
- Users must be able to disable the cache entirely (for reproducibility
  testing or when a provider's responses have changed).
- The cache key must be stable across runs (deterministic hash of canonical
  JSON).

## Decision

1. **Enable the disk cache by default.** `build_llm_cache` in `decon-cli`
   constructs a `DiskCache` unless `DECON_NO_CACHE=1` (or `true`) is set.

2. **LRU eviction with a size limit.** The default limit is **100 MB**
   (`DEFAULT_CACHE_SIZE_LIMIT_MB = 100` in `decon-cli`). The limit is
   configurable via `cache_size_limit_mb` in `decon.toml`.

3. **Eviction algorithm.** Every `EVICTION_CHECK_INTERVAL` (50) writes, the
   cache scans the cache directory, sums file sizes, and if the total exceeds
   the limit, deletes the oldest files (by `mtime`) until the total is under
   the limit. This is a simple, filesystem-based LRU:
   - Each cached response is a single file named by its SHA-256 key.
   - `mtime` is updated on every read (cache hit), so recently accessed entries
     survive eviction.
   - `CacheStats` tracks `hits`, `misses`, `evictions`, and
     `current_size_bytes` for observability.

4. **Cache location.** Default: platform cache directory (`dirs::cache_dir()`)
   + `/decon/llm-cache`. Override with `DECON_LLM_CACHE_DIR` env var or
   `cache_dir` in `decon.toml`.

5. **Bypass mechanism.** `DECON_NO_CACHE=1` disables the cache entirely —
   `build_llm_cache` returns `None`, and the provider client makes live calls
   on every request.

## Alternatives Considered

### Opt-in cache (status quo before M5)

- **Pros**: No surprise disk usage; users who want caching enable it
  explicitly.
- **Cons**: Most users do not know the cache exists, so they pay for
  identical re-runs. The nightly CI job had to document cache setup separately.
- **Rejected**: The default should optimize for the common case (iterative
  re-runs), not the rare case (reproducibility testing). Users who need no
  cache can set `DECON_NO_CACHE=1`.

### In-memory cache only (no disk persistence)

- **Pros**: No disk I/O; no unbounded growth concern.
- **Cons**: The cache is lost when the process exits, defeating the primary
  use case (re-run after inspecting output). Checkpoint resume across process
  boundaries would not benefit.
- **Rejected**: Disk persistence is essential for the iterative workflow.

### Exact-size eviction (evict on every write)

- **Pros**: Cache size is always at or below the limit.
- **Cons**: Scanning the directory on every write is expensive for large
  caches. The 50-write interval amortizes the cost while keeping the cache
  roughly bounded.
- **Rejected**: Too much I/O overhead on the critical path.

### TTL-based eviction (expire entries after N days)

- **Pros**: Bounded staleness — old entries are automatically refreshed.
- **Cons**: LLM providers may change models or behavior without notice, but a
  fixed TTL does not align with when those changes happen. The user is the
  best judge of when to refresh; `DECON_NO_CACHE=1` or deleting the cache dir
  is the explicit refresh mechanism.
- **Rejected**: Size-based LRU is simpler and sufficient; TTL adds a knob
  without clear value.

## Consequences

- **Positive**: Re-runs with unchanged prompts are free (cache hit). The
  nightly CI job costs less. Users do not need to configure caching — it
  works out of the box.
- **Positive**: The size limit prevents unbounded disk growth. The 100 MB
  default is generous for typical usage and configurable.
- **Negative**: A user who changes provider model behavior (e.g. a model
  update) may see stale cached responses until they clear the cache or set
  `DECON_NO_CACHE=1`. The cache key includes the model identifier, so changing
  `DECON_LLM_MODEL` automatically invalidates old entries — but a silent
  provider-side model change would not.
- **Negative**: The eviction scan (every 50 writes) adds a small I/O spike.
  For typical workloads this is negligible; for very large caches it may cause
  a brief pause.

## Related Documents

- `crates/decon-llm/src/cache.rs` — `DiskCache`, `CacheStats`, eviction logic.
- `crates/decon-cli/src/main.rs` — `build_llm_cache`, `cache_is_disabled`,
  `resolve_cache_root`, `DEFAULT_CACHE_SIZE_LIMIT_MB`.
- [ADR 0001](0001-checkpoint-schema-v1.md) — checkpoint format (complementary
  to cache).
- [ADR 0008](0008-two-tier-golden-fixture-strategy.md) — nightly LLM smoke
  relies on the cache.
- Issue #197 — disk cache default + LRU eviction.
- Issue #45 — original disk cache structure (M3).
