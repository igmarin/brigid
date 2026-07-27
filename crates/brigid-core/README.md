# brigid-core

[![crates.io](https://img.shields.io/crates/v/brigid-core.svg)](https://crates.io/crates/brigid-core)
[![docs.rs](https://docs.rs/brigid-core/badge.svg?version=latest)](https://docs.rs/brigid-core)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/igmarin/brigid/blob/main/LICENSE)

Pure domain models and pipeline traits for [`brigid`](https://github.com/igmarin/brigid).

This crate contains the heart of `brigid`'s logic: `Abstraction`, `Relationship`, `Chapter`, `RunConfig`, `Checkpoint`, budgeting, mermaid sanitization, setup-doc scoring, and scope filtering. It intentionally performs **no network or filesystem I/O**, so it stays fast and trivially unit-testable.

> This is an internal library crate. If you are looking for the end-user CLI, see the main [`brigid` README](https://github.com/igmarin/brigid/blob/main/README.md) or install `brigid-cli`.

---

## Usage

Add `brigid-core` to your `Cargo.toml`:

```toml
[dependencies]
brigid-core = "1"
```

Typical imports:

```rust
use brigid_core::{RunConfig, Abstraction, Relationship, mermaid::sanitize_mermaid};
```

See [docs.rs/brigid-core](https://docs.rs/brigid-core) for the full API.

---

## Project context

`brigid-core` is one crate in the `brigid` workspace:

- `brigid-core` — this crate: pure domain types and logic
- `brigid-crawl` — filesystem and GitHub repository crawling
- `brigid-llm` — LLM provider clients and caching
- `brigid-pipeline` — stage orchestration, checkpoint/resume, dry-run planning
- `brigid-cli` — the `brigid` binary

All crates are developed together in a single repository: <https://github.com/igmarin/brigid>.

---

## License

This project is licensed under the MIT License. See [LICENSE](https://github.com/igmarin/brigid/blob/main/LICENSE).
