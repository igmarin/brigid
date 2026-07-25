---
layout: default
title: "Migrating from Python"
nav_order: 5
---

# Migrating from the Python `decon` to the Rust `decon` CLI

The original `decon` was a Python/PocketFlow reference implementation. It has
been rewritten in Rust as a single, fast, distributable binary. The Python
entrypoint is now **deprecated** — no new features will be added to it, and it
will be removed in a future release.

This guide helps existing Python users switch to the Rust CLI with minimal
friction.

> **TL;DR** — Install the Rust binary (`brew install decon` or
> `cargo install decon-cli`), replace `python main.py` with `decon`, and update
> your environment variables. The pipeline behavior is the same; only the
> runtime changed.

---

## Why the migration happened

The Python implementation was excellent for rapid prompt iteration, but it
created friction for distribution and reliability:

| Problem in Python | How Rust solves it |
|-------------------|--------------------|
| Users need `pyenv`, a venv, and the correct Python version | Single static binary — no runtime to install |
| Dependency soup (`gemini`, `openai`, `pathspec`, `dotenv`, `pocketflow`) | One binary with audited dependencies (`cargo deny`) |
| Fragile runtime env (Make exporting empty `LLM_PROVIDER` overriding `.env`) | Typed config layers with explicit precedence; blank env vars are treated as unset |
| `shared` dict is a bag of keys — bugs are runtime-only | Typed domain models checked at compile time |
| Hard to ship as `brew install …` | Native Homebrew formula, `cargo install`, `cargo-binstall`, and GitHub Releases with pre-built binaries |
| Crawl performance on large trees | Parallel, gitignore-aware walk in Rust |

The **product value** — pipeline stages, prompt catalog, quality heuristics,
checkpoint/resume, monorepo scope — was preserved 1:1. The Rust CLI is a
faithful port of the Python pipeline, validated against a frozen
[`baseline.json`](../tests/fixtures/baseline.json) originally produced by the
Python reference. See [`move-to-rust.md`](move-to-rust.md) for the full
migration design.

---

## Installing the Rust binary

### Homebrew (macOS)

```bash
brew tap igmarin/homebrew-tap
brew install decon
```

This installs the `decon` binary, man page (`man 1 decon`), and shell
completions (bash, zsh, fish) automatically.

### cargo install

```bash
cargo install decon-cli
```

Or install directly from the git repository:

```bash
cargo install --git https://github.com/igmarin/decon-rs decon-cli
```

### cargo-binstall (fast binary download)

If you have [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall)
installed, `decon-cli` ships with binstall metadata so it downloads a
pre-built binary instead of compiling from source:

```bash
cargo binstall decon-cli
```

### Direct download

1. Go to the [Releases page](https://github.com/igmarin/decon-rs/releases).
2. Download the archive matching your platform, e.g.
   `decon-0.1.0-x86_64-unknown-linux-gnu.tar.gz`.
3. Verify the SHA-256 checksum against the `SHA256SUMS` file in the release.
4. Extract the archive and move the `decon` binary to your `PATH`:

   ```bash
   tar xzf decon-0.1.0-x86_64-unknown-linux-gnu.tar.gz
   sudo mv decon-0.1.0-x86_64-unknown-linux-gnu/decon /usr/local/bin/
   ```

5. Verify: `decon --version`

### Verifying checksums

Every release includes a `SHA256SUMS` file listing the SHA-256 hash of each
artifact. Verify a downloaded archive before installing:

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

---

## Command mapping

The Rust CLI uses a subcommand structure instead of `python main.py` with
positional arguments. Here is the mapping:

| Python command | Rust command | Notes |
|----------------|--------------|-------|
| `python main.py --dir PATH --model gpt-4o` | `decon generate --dir PATH` | Full pipeline. Model is set via env, not a flag. |
| `python main.py --repo URL` | `decon generate --repo URL` | GitHub repo fetch (same as Python) |
| `python main.py --dir PATH --language es` | `decon generate --dir PATH --language es` | i18n chrome (English + Spanish) |
| `make tutorial` | `decon generate --dir .` | Direct CLI call; Makefile wrapper still works |
| `make dry-run` | `decon dry-run --dir .` | Zero-LLM plan: crawl + scope + setup assessment |
| `make crawl` | `decon crawl --dir . --format json` | File inventory |
| `make eval` | `decon eval --out output/Project` | Structural quality gate |
| `python main.py --each-app` | `decon generate --dir . --each-app` | Per-app tutorials in monorepos |
| N/A | `decon resume --checkpoint PATH` | Resume from checkpoint (new) |
| N/A | `decon init --dir PATH` | Write starter `decon.toml` (new) |
| N/A | `decon identify --dir PATH` | Run identify stage only (new, for debugging) |
| N/A | `decon relationships --dir PATH` | Run relationships stage only (new) |

### Per-stage subcommands (new in Rust)

The Rust CLI exposes individual pipeline stages as subcommands for debugging:

```bash
decon identify --dir PATH
decon relationships --dir PATH --checkpoint-dir /tmp/ckpt
decon order --dir PATH --checkpoint-dir /tmp/ckpt
decon chapters --dir PATH --checkpoint-dir /tmp/ckpt
decon setup --dir PATH --checkpoint-dir /tmp/ckpt
decon overview --dir PATH --checkpoint-dir /tmp/ckpt
decon combine --dir PATH --checkpoint-dir /tmp/ckpt
```

These were not available in the Python implementation.

---

## Environment variable mapping

The Rust CLI uses a different set of environment variables. The key change:
**model and endpoint are now configured via environment variables, not CLI
flags**.

| Python env var | Rust env var | Purpose |
|----------------|--------------|---------|
| `OPENAI_API_KEY` | `DECON_LLM_API_KEY` | API key (checked first) |
| `GEMINI_API_KEY` | — | Not yet supported; use OpenAI-compatible providers |
| `LLM_PROVIDER` | — | Removed; the client is OpenAI-compatible, configured via `DECON_LLM_BASE_URL` |
| `OPENAI_API_BASE` | `DECON_LLM_BASE_URL` | Provider endpoint (default: `https://api.deepseek.com/v1`) |
| — | `DEEPSEEK_API_KEY` | Fallback API key (if `DECON_LLM_API_KEY` is unset) |
| — | `DECON_LLM_MODEL` | Model identifier (default: `deepseek-chat`) |
| — | `DECON_LLM_ALLOWED_HOSTS` | Extra hosts for Authorization-header allowlist |
| — | `DECON_LLM_CACHE_DIR` | Disk cache root for LLM responses |
| — | `DECON_NO_CACHE` | Set to `1` to disable disk cache |
| — | `DECON_FORCE_MOCK` | Force mock client (offline) |

### DeepSeek (default provider)

DeepSeek is the default — no extra configuration beyond the API key:

```bash
export DEEPSEEK_API_KEY="sk-your-key-here"
# or use the generic name (checked first):
export DECON_LLM_API_KEY="sk-your-key-here"
```

### OpenAI

```bash
export DECON_LLM_API_KEY="sk-your-openai-key"
export DECON_LLM_BASE_URL="https://api.openai.com/v1"
export DECON_LLM_MODEL="gpt-4o"
```

### Local providers (Ollama, LM Studio)

```bash
# Ollama
export DECON_LLM_API_KEY="ollama"
export DECON_LLM_BASE_URL="http://localhost:11434/v1"
export DECON_LLM_MODEL="llama3"
```

> **Important:** The Rust CLI never treats a blank/empty environment variable
> as set. This fixes a class of bugs from the Python/Make era where
> `LLM_PROVIDER=""` would silently override `.env` values.

---

## Configuration file

The Rust CLI supports a `decon.toml` configuration file (new — not in Python):

```bash
decon init --dir /path/to/project
```

This writes a starter `decon.toml` with documented defaults. Config precedence
is: **CLI flags > `decon.toml` > environment variables > built-in defaults**.

API keys are **never** written to `decon.toml` — only read from the
environment. A secret-field guard rejects `api_key` or `token` fields in
`decon.toml` at load time.

---

## Feature parity

| Feature | Python | Rust | Notes |
|---------|--------|------|-------|
| Local crawl (gitignore-aware) | ✅ | ✅ | Uses `ignore` crate |
| GitHub repo crawl | ✅ | ✅ | REST API + token |
| Monorepo scope (`--apps` / `--exclude-apps`) | ✅ | ✅ | |
| Context budgets (per-file truncate, per-batch) | ✅ | ✅ | |
| Map-reduce identify | ✅ | ✅ | Bounded concurrency via `tokio::Semaphore` |
| Relationships analysis | ✅ | ✅ | |
| Chapter ordering + writing | ✅ | ✅ | |
| Mermaid diagrams (sanitized/validated) | ✅ | ✅ | |
| Setup guide generation | ✅ | ✅ | Score-triggered |
| Architecture overview | ✅ | ✅ | Multi-app systems |
| i18n chrome (English + Spanish) | ✅ | ✅ | `--language en\|es` |
| Checkpoint / resume | ✅ | ✅ | Content-addressed (ADR 0001) + file-based storage (ADR 0006) |
| Ctrl+C graceful shutdown | Partial | ✅ | Saves checkpoint, exit code 5 |
| Dry-run (zero LLM) | ✅ | ✅ | `decon dry-run` |
| Eval (structural quality gate) | ✅ | ✅ | `decon eval` |
| `--each-app` monorepo fan-out | ✅ | ✅ | |
| `--review-chapters` polishing | ❌ | ✅ | Second LLM pass per chapter (new) |
| Per-stage subcommands | ❌ | ✅ | Debug individual stages (new) |
| `decon.toml` config file | ❌ | ✅ | Typed config with precedence (new) |
| `decon init` wizard | ❌ | ✅ | Write starter config (new) |
| `decon resume` status check | ❌ | ✅ | Inspect checkpoint without re-running (new) |
| Man page | ❌ | ✅ | `decon manpage` (new) |
| Shell completions | ❌ | ✅ | bash, zsh, fish, PowerShell (new) |
| Disk cache for LLM responses | ❌ | ✅ | Hash(prompt)+model+provider keyed (new) |
| Host allowlist for API keys | ❌ | ✅ | Prevents header leakage (new) |
| Exit codes | Ad hoc | ✅ | 0/1/2/3/4/5 — see [README](../README.md#exit-codes) |
| Pre-built binaries | ❌ | ✅ | Homebrew, cargo-binstall, GitHub Releases (new) |

---

## Exit codes

The Rust CLI maps outcomes to stable exit codes so CI and scripts can branch:

| Code | Meaning | What to do |
|------|---------|------------|
| `0` | Success | — |
| `1` | Generic failure | Check stderr |
| `2` | Config / path / I/O error | Verify `--dir` exists, `decon.toml` is valid |
| `3` | Budget exhausted | Raise `max_llm_calls` in `decon.toml` |
| `4` | LLM provider error | Network, timeout, rate-limit, or parse error |
| `5` | Cancelled (Ctrl+C / SIGTERM) | Partial checkpoint saved; re-run to resume |

The Python implementation used ad hoc exit codes. If you have CI scripts
checking `$?`, update them to the table above.

---

## FAQ

### Do I need to uninstall Python first?

No. The Rust binary is completely independent. You can install it alongside
the Python version and switch over at your own pace. Once you've verified the
Rust CLI works for your use cases, you can stop using the Python entrypoint.

### Will my existing output tutorials still work?

Yes. The output format (Markdown + Mermaid chapters, `index.md`, diagrams) is
the same. The Rust `decon eval` command can evaluate tutorials generated by
either implementation.

### Can I still use the Makefile?

Yes. Update your Makefile targets to call `decon` instead of
`python main.py`:

```makefile
tutorial:
	decon generate --dir . --output-dir output/$(PROJECT)

dry-run:
	decon dry-run --dir . --format json

eval:
	decon eval --out output/$(PROJECT)
```

### What happened to `LLM_PROVIDER`?

The Rust CLI uses a single OpenAI-compatible HTTP client. Instead of selecting
a provider by name, you set the base URL and model via environment variables
(`DECON_LLM_BASE_URL`, `DECON_LLM_MODEL`). This is simpler and works with any
OpenAI-compatible provider (DeepSeek, OpenAI, Ollama, LM Studio, vLLM, etc.).

### What happened to Gemini support?

Direct Gemini API support has not yet been ported. If your provider exposes an
OpenAI-compatible endpoint, point `DECON_LLM_BASE_URL` at it. Native Gemini
support may be added in a future release.

### How do I resume a failed run?

```bash
decon resume --checkpoint /path/to/checkpoint-dir --format json
```

This shows which stages completed. Re-running the same `decon generate` command
resumes from the last completed stage automatically — completed stages are
skipped.

### Where is the checkpoint stored?

In the `--checkpoint-dir` (default: a temp dir under the output directory). It
consists of `checkpoint.json` + `files.ndjson.gz` + per-stage output files with
SHA-256 verification. See [ADR 0001](adr/0001-checkpoint-schema-v1.md) and
[ADR 0006](adr/0006-file-based-checkpoint-output-storage.md).

### How do I force offline / mock mode?

```bash
export DECON_FORCE_MOCK=1
```

This forces the mock LLM client even when a real API key is present — useful
for offline testing and reproducibility.

### Is there a Python wrapper that calls the Rust binary?

Not yet. Option A (a thin Python wrapper package that delegates to the Rust
binary via `subprocess`) was considered but the Python code lives in a separate
repository. The recommended path is to switch to the Rust CLI directly. If a
Python wrapper is needed for backward compatibility, it can be built as a
separate package that shells out to `decon`.

### Where can I learn more about the migration design?

See [`move-to-rust.md`](move-to-rust.md) for the full migration design,
pipeline model, domain objects, phase plan, and engineering standards.
