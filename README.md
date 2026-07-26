# decon-rs
![Decon-RS Logo](https://github.com/user-attachments/assets/149638e7-1b52-4028-89c3-510a9383d15f)

[![CI](https://github.com/igmarin/decon-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/igmarin/decon-rs/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/igmarin/decon-rs/branch/main/graph/badge.svg)](https://codecov.io/gh/igmarin/decon-rs)
[![crates.io](https://img.shields.io/crates/v/decon-cli.svg)](https://crates.io/crates/decon-cli)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> Turn any codebase into a beginner-friendly tutorial — powered by LLMs, built in Rust.

`decon` crawls a codebase, identifies its core abstractions, and produces a
multi-chapter Markdown + Mermaid tutorial that explains how the system works:
setup, architecture, and inter-concept relationships. Built for monorepos and
large codebases where "read the source" is not a realistic onboarding path.

---

## How it works

```mermaid
flowchart LR
    A["Crawl\nfiles"] --> B["Identify\nabstractions"]
    B --> C["Map\nrelationships"]
    C --> D["Order\nchapters"]
    D --> E["Write chapters\n+ diagrams"]
    E --> F["Setup\nguide?"]
    F --> G["Architecture\noverview?"]
    G --> H["Combine\nindex"]
    H --> I["Tutorial\noutput"]
```

Every expensive stage is **checkpointed** — interrupt with Ctrl+C and re-run
to resume from the last completed stage. No work is lost.

---

## Install

```bash
# Homebrew (macOS)
brew tap igmarin/homebrew-tap
brew install decon

# cargo install
cargo install decon-cli

# cargo-binstall (pre-built binary)
cargo binstall decon-cli
```

Or download a binary from
[GitHub Releases](https://github.com/igmarin/decon-rs/releases).

Verify: `decon --version`

---

## Quick start

```bash
# 1. Set your LLM API key (DeepSeek is the default provider)
export DEEPSEEK_API_KEY="sk-your-key-here"

# 2. Generate a tutorial from any codebase
decon generate --dir ./my-project --output-dir ./tutorial

# 3. Open the result
open ./tutorial/index.md
```

That's it. The tutorial is plain Markdown with Mermaid diagrams — render it
in any Markdown viewer (GitHub, VS Code, Obsidian).

> **No API key?** Set `DECON_FORCE_MOCK=1` to run the full pipeline with a
> mock LLM client for offline testing.

---

## What you get

```mermaid
flowchart TB
    subgraph "Tutorial output"
        IDX["index.md\n— table of contents\n— system overview\n— navigation links"]
        CH["chapters/\n— one file per abstraction\n— Mermaid diagrams\n— real file paths"]
        SETUP["setup.md\n— local setup steps\n— gap assessment"]
        OVERVIEW["overview.md\n— architecture diagram\n— cross-cutting concerns"]
    end
```

Each chapter references **real file paths** from your repo and includes
**Mermaid diagrams** that visualize the concept. The setup guide fills gaps
when official docs are thin. The architecture overview ties everything
together for monorepos and multi-app systems.

---

## Key features

- **Full pipeline** — crawl → identify → relationships → order → chapters →
  setup → overview → combine
- **JSON output** — `--format json` on every stage for CI and editor plugins
- **Incremental** — `--since <git-ref>` only re-analyzes changed files
- **Monorepo support** — `--each-app` generates one tutorial per app
- **i18n** — `--language en|es` localizes tutorial chrome
- **Chapter review** — `--review-chapters` adds a second LLM pass per chapter
- **Checkpoint + resume** — interrupt and resume without losing work
- **Disk cache** — LLM responses cached by default; re-runs are free
- **Plugins** — custom kind detectors via `KindDetector` trait (ADR 0014)

---

## CLI at a glance

| Command | What it does |
|---------|-------------|
| `decon generate` | Full pipeline → tutorial |
| `decon crawl` | File inventory (zero LLM) |
| `decon dry-run` | Plan + budget (zero LLM) |
| `decon eval` | Tutorial quality gate (zero LLM) |
| `decon identify` | Single stage: abstraction identification |
| `decon relationships` | Single stage: relationship analysis |
| `decon order` | Single stage: chapter ordering |
| `decon chapters` | Single stage: chapter writing |
| `decon setup` | Single stage: setup guide |
| `decon overview` | Single stage: architecture overview |
| `decon combine` | Single stage: index assembly |
| `decon init` | Write a starter `decon.toml` |
| `decon resume` | Checkpoint status report |
| `decon completions` | Generate shell completions |
| `decon manpage` | Generate a man page |

For every flag, environment variable, and provider configuration, see the
[Usage Guide](docs/usage-guide.md).

---

## Configuration

`decon init` writes a starter `decon.toml`. Precedence is
**CLI > file > env > defaults**.

```toml
max_llm_calls = 200
max_abstractions = 30

[plugins]
dirs = ["./plugins"]
```

Run `decon init --check` to validate an existing config.

---

## Troubleshooting

| Exit code | Meaning |
|-----------|---------|
| `0` | Success |
| `1` | Generic failure |
| `2` | Config / path / I/O error |
| `3` | Budget exhausted |
| `4` | LLM provider error |
| `5` | Cancelled — re-run to resume |

For recovery procedures, common issues, and fixes, see
[Troubleshooting](docs/troubleshooting.md).

---

## Documentation

| Document | What it covers |
|----------|---------------|
| [Usage Guide](docs/usage-guide.md) | Every command, flag, env var, provider setup, examples |
| [Troubleshooting](docs/troubleshooting.md) | Exit codes, recovery, common issues |
| [Project Status](docs/project-status.md) | Milestones, what works, roadmap |
| [Architecture](ARCHITECTURE.md) | Crate structure, data flow, design principles, ADR index |
| [Contributing](CONTRIBUTING.md) | TDD workflow, CI checks, PR process, commit conventions |
| [Changelog](CHANGELOG.md) | Release history per milestone |
| [Best Practices](docs/best-practices.md) | Tutorial quality rules (scope, budget, mermaid) |
| [Migration from Python](docs/migrating-from-python.md) | Guide for Python `decon` users |
| [Move to Rust](docs/move-to-rust.md) | Migration design, pipeline model, phase plan |
| [ADRs](docs/adr/) | Architecture Decision Records (0001–0016) |

---

## License

MIT
