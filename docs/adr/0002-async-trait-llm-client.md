# ADR 0002: async-trait for the LlmClient Trait

## Status

Accepted

## Date

2026-07-24

## Context

`decon` talks to LLM providers (OpenAI-compatible first, per the project's
provider priority) behind a single provider-agnostic interface. Milestone 3
introduces the `LlmClient` trait in `decon-llm` so that the pipeline can call
any provider — or a `MockClient` test double — through one object-safe type.

The trait is intentionally minimal: a single `async fn complete(&self, prompt:
&str) -> Result<String, LlmError>` method. The concrete implementations that
land in M3/M4 are:

- `MockClient` (`crates/decon-llm/src/mock.rs`) — a thread-safe, network-free
  test double backed by a `Mutex<MockState>`.
- `OpenAiCompatibleClient` (`crates/decon-llm/src/openai_client.rs`) — an HTTP
  client using `reqwest` with retry/backoff/timeout and optional `DiskCache`.

Both are used as `&dyn LlmClient` for dependency injection in tests and in the
bounded-concurrency fan-out (`crates/decon-llm/src/concurrency.rs`), which takes
`client: &dyn LlmClient`. Object safety is therefore a hard requirement: the
pipeline must be able to hold a single `dyn LlmClient` and swap providers
without recompiling call sites.

The `Send + Sync` supertrait is required because `bounded_complete` shares the
`&dyn LlmClient` reference across spawned tasks that each acquire a semaphore
permit and call `complete` concurrently.

### The language constraint

Rust's native `async fn` in traits was stabilized in 1.75, but a trait with
`async fn` methods is **not object-safe** by default — `dyn LlmClient` would not
compile because the compiler cannot synthesize a vtable for an associated
future type whose size and lifetime are not known at the call site. The
workarounds each have tradeoffs:

1. **Return `Pin<Box<dyn Future<...>>>` (BoxFuture) by hand.** Object-safe, but
   every implementation must manually box the future and spell out the `Pin`,
   `Box`, and lifetime parameters. The trait signature becomes noisy and every
   call site pays a heap allocation per invocation.
2. **Use `async-trait`.** The macro rewrites `async fn` in the trait into a
   `Pin<Box<dyn Future + Send>>` return behind the scenes, keeping the trait
   definition readable (`async fn complete(...)`) while preserving object
   safety. Implementations also use `async fn` syntax. The same heap allocation
   happens, but it is hidden by the macro and the trait/impl source stays clean.
3. **Wait for native object-safe async traits** (e.g. `trait_upcaster` /
   `dyn*` / `async-fn-in-trait` object-safety work). Not stable as of the
   Rust toolchain pinned for M3; relying on it would block the milestone.

## Decision

Use the `async-trait` crate (0.1) for the `LlmClient` trait and all its
implementations.

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, prompt: &str) -> Result<String, LlmError>;
}
```

This is exactly what `crates/decon-llm/src/client.rs` declares. The dependency
is declared in `crates/decon-llm/Cargo.toml`:

```toml
async-trait = "0.1"
```

Every implementation — `MockClient`, `OpenAiCompatibleClient`, and the
`ConcurrencyTracker` test helper in `concurrency.rs` — applies
`#[async_trait]` to its `impl LlmClient` block and writes the method body in
plain `async fn` syntax.

The `Send + Sync` supertrait is kept so that `&dyn LlmClient` can be shared
across tokio tasks in `bounded_complete`.

## Alternatives Considered

### Hand-rolled `BoxFuture` return type

- **Pros**: No proc-macro dependency; the trait is object-safe without any
  external crate.
- **Cons**: The trait signature becomes
  `fn complete<'a>(&'a self, prompt: &'a str) -> Pin<Box<dyn Future<Output = Result<String, LlmError>> + Send + 'a>>;`
  and every implementation must manually `Box::pin(async move { ... })`. This is
  noisy, error-prone for lifetimes, and makes the trait harder to read for new
  contributors. The allocation cost is identical to `async-trait`.
- **Rejected**: The readability and ergonomics cost is not justified given that
  `async-trait` produces the same object-safe desugaring automatically.

### Native `async fn` in trait (no macro)

- **Pros**: No external dependency; uses the stable 1.75 feature directly.
- **Cons**: The trait is not object-safe, so `dyn LlmClient` does not compile.
  The pipeline and the bounded-concurrency module both rely on `&dyn
  LlmClient` for dependency injection and provider swappability. Switching to
  generics (`impl LlmClient`) would propagate the type parameter through every
  pipeline stage and make mock injection in tests significantly harder.
- **Rejected**: Object safety is a hard requirement for this project's
  dependency-injection and test-double pattern.

### `trait_upcaster` / `dyn*` / future object-safety features

- **Pros**: Would eventually allow native object-safe async traits with no
  allocation.
- **Cons**: Not stable on the Rust toolchain pinned for M3. Pinning the
  project's MSRV to a nightly feature would break the `cargo deny` / CI
  toolchain guarantees and is not acceptable for a security-sensitive CLI.
- **Rejected**: Not available on stable Rust.

## Consequences

- **Positive**: The `LlmClient` trait stays readable (`async fn complete`) while
  remaining object-safe, so `dyn LlmClient` works for dependency injection in
  the pipeline and in tests (`Box<dyn LlmClient>` is exercised by the
  `works_as_dyn_llm_client` test in `mock.rs`).
- **Positive**: All implementations (`MockClient`, `OpenAiCompatibleClient`,
  test helpers) use the same `async fn` syntax, keeping the codebase
  consistent and easy to extend with new providers.
- **Positive**: The `Send + Sync` supertrait composes cleanly with the
  semaphore-based bounded concurrency in `concurrency.rs`, which shares
  `&dyn LlmClient` across tasks.
- **Negative**: `async-trait` introduces a heap allocation (`Box::pin`) per
  `complete` call. For LLM calls that are network-bound and measured in
  hundreds of milliseconds to seconds, this allocation is negligible.
- **Negative**: `async-trait` is a proc-macro dependency that must be kept in
  `Cargo.toml` and audited by `cargo deny`. It is widely used (tokio, reqwest
  ecosystem), well-maintained, and MIT/Apache-2.0 licensed, so the supply-chain
  risk is low.
- **Negative**: When Rust eventually stabilizes native object-safe async traits,
  migrating off `async-trait` will require touching every `impl LlmClient`
  block. The trait surface is small (one method), so the migration cost is
  bounded.

## Related Documents

- `docs/adr/0001-checkpoint-schema-v1.md` — the checkpoint ADR whose resume
  behavior depends on the pipeline stages that call `LlmClient`.
- `crates/decon-llm/src/client.rs` — the `LlmClient` trait definition.
- `crates/decon-llm/src/mock.rs` — `MockClient` test double implementing the
  trait.
- `crates/decon-llm/src/openai_client.rs` — `OpenAiCompatibleClient`
  implementing the trait.
- `crates/decon-llm/src/concurrency.rs` — `bounded_complete` consuming
  `&dyn LlmClient`.
- `crates/decon-llm/Cargo.toml` — `async-trait = "0.1"` dependency.
