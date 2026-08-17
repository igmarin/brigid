# brigid-llm

> **Deprecated (brigid 2.0.0).** This crate is superseded by
> [`llm-kernel`](https://crates.io/crates/llm-kernel). Use
> `llm_kernel::llm::LLMClient` / `OpenAIClient` / `CacheClient` for new code.
> `brigid-llm` remains in the workspace until Phase 4 of
> [issue #297](https://github.com/igmarin/brigid/issues/297). Bug fixes only.

[![crates.io](https://img.shields.io/crates/v/brigid-llm.svg)](https://crates.io/crates/brigid-llm)
[![docs.rs](https://docs.rs/brigid-llm/badge.svg?version=latest)](https://docs.rs/brigid-llm)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/igmarin/brigid/blob/main/LICENSE)

LLM provider clients and the `LlmClient` trait for [`brigid`](https://github.com/igmarin/brigid).

This crate provides a provider-agnostic `LlmClient` interface, an OpenAI-compatible implementation, bounded concurrency with budget enforcement, retries/backoff, and a disk cache keyed by `hash(prompt) + model + provider`. It is built on `reqwest` and `tokio`.

> This is an internal library crate. If you are looking for the end-user CLI, see the main [`brigid` README](https://github.com/igmarin/brigid/blob/main/README.md) or install `brigid-cli`.

---

## Usage (legacy — deprecated)

> The following usage section refers to the **deprecated** `brigid-llm` API.
> New code should use [`llm-kernel`](https://crates.io/crates/llm-kernel)
> (`llm_kernel::llm::LLMClient`, `OpenAIClient`, `CacheClient`) instead.
> `brigid-llm` remains only as a CLI compatibility bridge until Phase 4.

Add `brigid-llm` to your `Cargo.toml` (legacy only):

```toml
[dependencies]
brigid-llm = "2"
```

Use the OpenAI-compatible client (legacy):

```rust
use brigid_llm::{OpenAiCompatibleClient, OpenAiClientConfig, LlmClient};

let config = OpenAiClientConfig::default();
let client = OpenAiCompatibleClient::new(config);
// client.complete(...).await
```

See [docs.rs/brigid-llm](https://docs.rs/brigid-llm) for the full API.

---

## Project context

`brigid-llm` is one crate in the `brigid` workspace:

- `brigid-core` — pure domain types and logic
- `brigid-crawl` — filesystem and GitHub repository crawling
- `brigid-llm` — this crate: LLM provider clients and caching
- `brigid-pipeline` — stage orchestration, checkpoint/resume, dry-run planning
- `brigid-cli` — the `brigid` binary

All crates are developed together in a single repository: <https://github.com/igmarin/brigid>.

---

## License

This project is licensed under the MIT License. See [LICENSE](https://github.com/igmarin/brigid/blob/main/LICENSE).
