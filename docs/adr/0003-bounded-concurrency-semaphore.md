# ADR 0003: Bounded Concurrency with tokio::sync::Semaphore

## Status

Accepted

## Date

2026-07-24

## Context

`decon`'s pipeline stages fan out LLM calls in map/reduce batches. A single
"identify" or "chapter" stage can issue dozens of `LlmClient::complete` calls
against the same provider. Running all of them concurrently risks:

- **Rate-limit exhaustion** — providers return 429s and the retry/backoff
  storm amplifies load instead of reducing it.
- **Connection-pool saturation** — `reqwest` reuses connections, but an
  unbounded fan-out can open more concurrent connections than the pool or the
  OS file-descriptor table can handle.
- **Budget blow-by** — without a concurrency cap, a large batch can have many
  calls in flight before the `ProgressTracker` budget check has a chance to
  stop the stage, making cost control coarser than intended.

The project's review prompt (`.github/review-prompt.md`) explicitly flags
"unbounded `join_all` / spawn loop over provider calls" as a blocking
architecture finding. So the concurrency mechanism must be bounded, and the
bound must be configurable per batch.

Additionally, the `max-llm-calls` budget (`decon-core::ProgressTracker`) must
be enforced **before** fanning out, so that a batch that would exceed the
remaining budget fails fast and no calls are made — this is the fail-closed
discipline required by the product spec.

## Decision

Use a `tokio::sync::Semaphore` with `max_concurrency` permits to cap the number
of concurrent in-flight `complete` calls. This is implemented in
`crates/decon-llm/src/concurrency.rs`.

### `bounded_complete`

```rust
pub async fn bounded_complete(
    client: &dyn LlmClient,
    prompts: Vec<String>,
    max_concurrency: usize,
) -> Vec<Result<String, LlmError>> {
    let max = max_concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(max));

    let futures = prompts.into_iter().map(|prompt| {
        let sem = Arc::clone(&semaphore);
        async move {
            let _permit = sem.acquire_owned().await
                .map_err(|_| LlmError::network("concurrency semaphore closed unexpectedly"))?;
            client.complete(&prompt).await
        }
    });

    join_all(futures).await
}
```

Key design points, grounded in the actual implementation:

- **`max_concurrency = 0` is treated as `1`.** A semaphore with zero permits
  would deadlock (no permit can ever be acquired), so `max_concurrency.max(1)`
  normalizes the input. This is tested by `max_concurrency_zero_treated_as_one`
  in `concurrency.rs`. The choice of `1` (rather than erroring) keeps the API
  ergonomic: a caller that passes `0` gets sequential execution, not a panic.
- **`join_all` preserves input order.** Results are returned in the same order
  as the input `prompts`, so the caller does not need to re-sort. Each future
  acquires its permit independently; `join_all` polls them all but the
  semaphore ensures at most `max` are past the `acquire_owned` point at once.
- **A failure in one call does not abort the others.** All calls are
  attempted; errors are returned in the corresponding position. This is tested
  by `one_failure_does_not_abort_others`. Aborting on first error would waste
  already-in-flight work and make partial-result handling harder for the
  pipeline.
- **The semaphore is never closed**, so `acquire_owned` cannot fail in
  practice. The error path is still handled gracefully (returning
  `LlmError::network`) rather than panicking, because this is library code.

### `bounded_complete_with_budget`

```rust
pub async fn bounded_complete_with_budget(
    client: &dyn LlmClient,
    prompts: Vec<String>,
    max_concurrency: usize,
    progress: &mut ProgressTracker,
) -> Result<Vec<Result<String, LlmError>>, BudgetExceeded> {
    let n = u32::try_from(prompts.len()).unwrap_or(u32::MAX);
    progress.reserve_llm_calls(n)?;
    Ok(bounded_complete(client, prompts, max_concurrency).await)
}
```

The budget is enforced **before** fanning out:

- `ProgressTracker::reserve_llm_calls(n)` checks whether `used + n > max` and
  increments `used` atomically. If the batch would exceed the ceiling, a single
  `BudgetExceeded` is returned immediately and **no calls are made** (tested by
  `budget_exceeded_returns_error`, which asserts `client.call_count() == 0`).
- Reserving up front both checks the budget **and** increments the used count,
  so callers must not additionally call `record_llm_call` for the same batch —
  that would double-count. This is documented in the function's rustdoc.
- An empty `prompts` vec returns an empty result vec immediately and does not
  touch the budget (tested by `budget_empty_prompts_succeeds`, which passes
  even with `max_llm_calls = 0`).

## Alternatives Considered

### `FuturesUnordered` with manual concurrency control

- **Pros**: No semaphore; stream results as they complete, which could let the
  caller start reduce work earlier.
- **Cons**: `FuturesUnordered` does not inherently bound concurrency — all
  futures are polled. Bounding would still require a semaphore or a channel
  to gate spawning, so the semaphore is the primitive either way. The
  order-preserving guarantee of `join_all` is also lost, requiring the caller
  to re-sort results by index.
- **Rejected**: Does not remove the need for a concurrency primitive and
  sacrifices input-order preservation.

### Bounded MPSC channel as a worker pool

- **Pros**: A fixed pool of worker tasks pulling from a channel naturally
  bounds concurrency and could reuse tasks across batches.
- **Cons**: Significantly more machinery (spawn N workers, send prompts
  through a channel, collect results through a second channel, handle
  shutdown). For a per-batch fan-out where the batch size is known up front,
  the semaphore + `join_all` approach is simpler and has equivalent
  semantics. A persistent worker pool would also complicate the `&dyn
  LlmClient` lifetime story and the budget integration.
- **Rejected**: The added complexity is not justified for the current per-batch
  fan-out pattern.

### `Buffer`/`buffer_unordered` from `tokio-stream`

- **Pros**: Combinator-based, idiomatic for stream pipelines.
- **Cons**: Requires converting the prompt vec into a stream and the results
  back into a vec, adding adapter boilerplate. `buffer_unordered` also does
  not preserve input order, which the pipeline relies on.
- **Rejected**: More boilerplate for no functional gain over the semaphore
  approach.

## Consequences

- **Positive**: Concurrency is bounded by a single, explicit parameter
  (`max_concurrency`), making rate-limit and connection-pool management
  predictable and configurable per batch.
- **Positive**: The `max_concurrency = 0 → 1` normalization prevents a
  footgun deadlock without requiring every caller to validate the input.
- **Positive**: Budget enforcement happens before any call is made, so a batch
  that would exceed the `max-llm-calls` ceiling fails fast with zero wasted
  LLM spend (fail-closed discipline).
- **Positive**: Results are returned in input order, simplifying the caller's
  reduce step.
- **Positive**: A single call failure does not cancel the batch, so partial
  results are available for the pipeline to handle gracefully.
- **Negative**: `join_all` holds all futures in memory simultaneously, so
  memory for the batch is O(n) in the number of prompts. For the batch sizes
  in this project (tens to low hundreds), this is acceptable; a future
  streaming-batch stage could revisit this if batch sizes grow significantly.
- **Negative**: The budget is reserved for the entire batch up front, which
  means a batch of `n` prompts consumes `n` budget units even if some calls
  fail. This is intentional (the calls were attempted) but means a failed
  batch does not refund its budget. Callers that want per-call budgeting
  should use smaller batches.

## Related Documents

- `docs/adr/0001-checkpoint-schema-v1.md` — checkpoint resume depends on
  pipeline stages that use bounded concurrency.
- `docs/adr/0002-async-trait-llm-client.md` — the `LlmClient` trait that
  `bounded_complete` operates on.
- `crates/decon-llm/src/concurrency.rs` — the implementation.
- `crates/decon-core/src/progress.rs` — `ProgressTracker` and
  `BudgetExceeded`.
- `.github/review-prompt.md` — flags unbounded LLM fan-out as a blocking
  finding.
