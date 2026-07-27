# brigid-crawl

[![crates.io](https://img.shields.io/crates/v/brigid-crawl.svg)](https://crates.io/crates/brigid-crawl)
[![docs.rs](https://docs.rs/brigid-crawl/badge.svg?version=latest)](https://docs.rs/brigid-crawl)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/igmarin/brigid/blob/main/LICENSE)

Local filesystem and GitHub repository crawling for [`brigid`](https://github.com/igmarin/brigid).

This crate produces a scoped, gitignore-aware file inventory that the rest of the `brigid` pipeline uses as its starting input. It supports incremental crawling via `git diff` and is symlink-safe.

> This is an internal library crate. If you are looking for the end-user CLI, see the main [`brigid` README](https://github.com/igmarin/brigid/blob/main/README.md) or install `brigid-cli`.

---

## Usage

Add `brigid-crawl` to your `Cargo.toml`:

```toml
[dependencies]
brigid-crawl = "1"
```

Crawl a local directory:

```rust
use brigid_crawl::{CrawlOptions, crawl_local};
use std::path::PathBuf;

let options = CrawlOptions {
    root: PathBuf::from("."),
    ..Default::default()
};
let result = crawl_local(&options).expect("crawl should succeed");
```

See [docs.rs/brigid-crawl](https://docs.rs/brigid-crawl) for the full API.

---

## Project context

`brigid-crawl` is one crate in the `brigid` workspace:

- `brigid-core` — pure domain types and logic
- `brigid-crawl` — this crate: filesystem and GitHub repository crawling
- `brigid-llm` — LLM provider clients and caching
- `brigid-pipeline` — stage orchestration, checkpoint/resume, dry-run planning
- `brigid-cli` — the `brigid` binary

All crates are developed together in a single repository: <https://github.com/igmarin/brigid>.

---

## License

This project is licensed under the MIT License. See [LICENSE](https://github.com/igmarin/brigid/blob/main/LICENSE).
