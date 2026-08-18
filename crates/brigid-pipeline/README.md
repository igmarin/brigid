# brigid-pipeline

[![crates.io](https://img.shields.io/crates/v/brigid-pipeline.svg)](https://crates.io/crates/brigid-pipeline)
[![docs.rs](https://docs.rs/brigid-pipeline/badge.svg?version=latest)](https://docs.rs/brigid-pipeline)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/igmarin/brigid/blob/main/LICENSE)

Stage orchestration, checkpoint/resume, and dry-run planning for [`brigid`](https://github.com/igmarin/brigid).

This crate implements the `brigid` generate pipeline as a linear, checkpointed state machine: identify abstractions, map relationships, order chapters, write chapters, generate a setup guide, produce an architecture overview, and combine everything into a final tutorial index. It also renders the Jinja2 prompt templates stored in `prompts/`.

> This is an internal library crate. If you are looking for the end-user CLI, see the main [`brigid` README](https://github.com/igmarin/brigid/blob/main/README.md) or install `brigid-cli`.

---

## Usage

Add `brigid-pipeline` to your `Cargo.toml`:

```toml
[dependencies]
brigid-pipeline = "1"
```

Run a dry-run plan:

```rust
use brigid_pipeline::dry_run;

let plan = dry_run("./my-project").expect("dry-run should succeed");
```

See [docs.rs/brigid-pipeline](https://docs.rs/brigid-pipeline) for the full API.

---

## Prompt templates

The Jinja2 prompt templates used by the LLM stages live in [`crates/brigid-pipeline/prompts/`](https://github.com/igmarin/brigid/tree/main/crates/brigid-pipeline/prompts). They are embedded at compile time with `include_str!`, so the published crate has no runtime dependency on the prompt directory layout.

---

## Project context

`brigid-pipeline` is one crate in the `brigid` workspace:

- `brigid-core` — pure domain types and logic
- `brigid-crawl` — filesystem and GitHub repository crawling
- `brigid-pipeline` — this crate: stage orchestration, checkpoint/resume, LLM client wiring
- `brigid-mcp` — MCP server for codebase knowledge querying
- `brigid-cli` — the `brigid` binary

All crates are developed together in a single repository: <https://github.com/igmarin/brigid>.

---

## License

This project is licensed under the MIT License. See [LICENSE](https://github.com/igmarin/brigid/blob/main/LICENSE).
