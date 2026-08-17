# brigid-cli

[![crates.io](https://img.shields.io/crates/v/brigid-cli.svg)](https://crates.io/crates/brigid-cli)
[![docs.rs](https://docs.rs/brigid-cli/badge.svg?version=latest)](https://docs.rs/brigid-cli)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/igmarin/brigid/blob/main/LICENSE)

The `brigid` command-line tool: deconstruct a codebase into an AI-generated tutorial.

`brigid` crawls a codebase, identifies its core abstractions via an LLM map/reduce pipeline, and produces a multi-chapter Markdown + Mermaid tutorial explaining how the system works: setup, architecture, and inter-concept relationships. Built for monorepos and large codebases where "read the source" is not a realistic onboarding path.

This is the only binary crate in the `brigid` workspace. The business logic lives in the companion library crates (`brigid-core`, `brigid-crawl`, `brigid-pipeline`, plus deprecated `brigid-llm`); live completions go through [`llm-kernel`](https://crates.io/crates/llm-kernel). `brigid-cli` is a thin wrapper that parses arguments, wires the pipeline, and maps errors to exit codes.

---

## Install

```bash
# cargo install
cargo install brigid-cli

# cargo-binstall (pre-built Linux binary, compiles from source on macOS/Windows)
cargo binstall brigid-cli
```

Or download the Linux binary from [GitHub Releases](https://github.com/igmarin/brigid/releases).

Verify: `brigid --version`

---

## Quick start

```bash
# 1. Set your LLM API key (DeepSeek is the default provider)
export DEEPSEEK_API_KEY="sk-your-key-here"

# 2. Generate a tutorial from any codebase
brigid generate --dir ./my-project --output-dir ./tutorial

# 3. Open the result
open ./tutorial/index.md
```

The tutorial is plain Markdown with Mermaid diagrams — render it in any Markdown viewer (GitHub, VS Code, Obsidian).

> **No API key?** Set `BRIGID_FORCE_MOCK=1` to run the full pipeline with a mock LLM client for offline testing.

---

## Common commands

```bash
brigid init                         # write a starter brigid.toml
brigid crawl --dir ./my-project     # list scoped files (no LLM)
brigid dry-run --dir ./my-project   # plan without calling an LLM
brigid generate --dir ./my-project  # run the full pipeline
brigid eval --out ./tutorial        # structural quality check
brigid resume --checkpoint .brigid-checkpoint
```

See `brigid --help` and `brigid <command> --help` for all options.

---

## Project context

`brigid-cli` is one crate in the `brigid` workspace:

- `brigid-core` — pure domain types and logic
- `brigid-crawl` — filesystem and GitHub repository crawling
- `brigid-llm` — deprecated; pipeline uses `llm-kernel`
- `brigid-pipeline` — stage orchestration, checkpoint/resume, dry-run planning
- `brigid-cli` — this crate: the `brigid` binary

All crates are developed together in a single repository: <https://github.com/igmarin/brigid>.

For the full guide, diagrams, and development docs, see the main [`brigid` README](https://github.com/igmarin/brigid/blob/main/README.md).

---

## License

This project is licensed under the MIT License. See [LICENSE](https://github.com/igmarin/brigid/blob/main/LICENSE).
