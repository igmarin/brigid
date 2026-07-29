# Usage Guide

A deep dive into every `brigid` command, flag, environment variable, and
provider configuration. If you just want to get started quickly, see the
[`README.md`](../README.md) first.

---

## Installation

`brigid` ships a pre-built binary for Linux x86_64. macOS and Windows users
install from source with Homebrew or `cargo install`.

### Homebrew (macOS)

```bash
brew tap igmarin/homebrew-tap
brew install brigid
```

Installs the `brigid` binary, man page (`man 1 brigid`), and shell completions
(bash, zsh, fish) automatically.

### cargo install

```bash
cargo install brigid-cli
```

Or directly from the repository:

```bash
cargo install --git https://github.com/igmarin/brigid brigid-cli
```

### cargo-binstall (pre-built binary)

If you have [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall),
`brigid-cli` ships with binstall metadata so it downloads a pre-built binary
instead of compiling from source:

```bash
cargo binstall brigid-cli
```

### Direct download

1. Go to the [Releases page](https://github.com/igmarin/brigid/releases).
2. Download the archive matching your platform, e.g.
   `brigid-0.1.0-x86_64-unknown-linux-gnu.tar.gz`.
3. Verify the SHA-256 checksum against the `SHA256SUMS` file in the release.
4. Extract and move the `brigid` binary to your `PATH`:

   ```bash
   tar xzf brigid-0.1.0-x86_64-unknown-linux-gnu.tar.gz
   sudo mv brigid-0.1.0-x86_64-unknown-linux-gnu/brigid /usr/local/bin/
   # Optional: install man page and completions
   sudo mv brigid-0.1.0-x86_64-unknown-linux-gnu/brigid.1 /usr/local/share/man/man1/
   mkdir -p ~/.local/share/bash-completion/completions
   mv brigid-0.1.0-x86_64-unknown-linux-gnu/completions/brigid.bash \
      ~/.local/share/bash-completion/completions/brigid
   ```

5. Verify: `brigid --version`

### Verifying checksums

Every release includes a `SHA256SUMS` file. Verify a downloaded archive
before installing:

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

---

## API key setup

The `generate` and `identify` stages call an LLM. `brigid` uses an
OpenAI-compatible HTTP client, so any provider that exposes that API works.

### DeepSeek (default / primary)

DeepSeek is the default provider — no extra configuration beyond the API key.

1. Create an account at <https://platform.deepseek.com> and generate an API
   key.
2. Export it before running `brigid`:

   ```bash
   export DEEPSEEK_API_KEY="sk-your-key-here"
   # or use the generic name (checked first):
   export BRIGID_LLM_API_KEY="sk-your-key-here"
   ```

`BRIGID_LLM_API_KEY` takes precedence over `DEEPSEEK_API_KEY`; either is
accepted. The key is **never** written to `brigid.toml` — only read from the
environment.

### OpenAI

Point the client at the OpenAI endpoint and pick a model:

```bash
export BRIGID_LLM_API_KEY="sk-your-openai-key"
export BRIGID_LLM_BASE_URL="https://api.openai.com/v1"
export BRIGID_LLM_MODEL="gpt-4o"
```

`api.openai.com` is in the built-in host allowlist, so no extra host
configuration is needed.

### Local providers (Ollama, LM Studio)

Any OpenAI-compatible local server works. Set the base URL to the local
endpoint and add the host to the allowlist if it is not already covered by
the `localhost` / `127.0.0.1` defaults.

```bash
# Ollama (default port 11434)
export BRIGID_LLM_API_KEY="ollama"          # any non-empty string
export BRIGID_LLM_BASE_URL="http://localhost:11434/v1"
export BRIGID_LLM_MODEL="llama3"

# LM Studio (default port 1234)
export BRIGID_LLM_API_KEY="lm-studio"
export BRIGID_LLM_BASE_URL="http://localhost:1234/v1"
export BRIGID_LLM_MODEL="local-model"
```

For a non-loopback host, extend the allowlist with
`BRIGID_LLM_ALLOWED_HOSTS` (comma-separated) or the `[[allowed_hosts]]` table
in `brigid.toml`:

```bash
export BRIGID_LLM_ALLOWED_HOSTS="my-proxy.internal,10.0.0.5"
```

---

## Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `BRIGID_LLM_API_KEY` | — | API key (checked first; falls back to `DEEPSEEK_API_KEY`) |
| `DEEPSEEK_API_KEY` | — | API key (fallback) |
| `BRIGID_LLM_BASE_URL` | `https://api.deepseek.com/v1` | OpenAI-compatible endpoint |
| `BRIGID_LLM_MODEL` | `deepseek-chat` | Model identifier sent in requests |
| `BRIGID_LLM_ALLOWED_HOSTS` | — | Extra hosts for the Authorization-header allowlist (comma-separated) |
| `BRIGID_LLM_CACHE_DIR` | platform cache `/brigid/llm-cache` | Disk cache root for LLM responses |
| `BRIGID_NO_CACHE` | — | Set to `1` / `true` to disable the disk cache |
| `BRIGID_FORCE_MOCK` | — | Set to force the mock client (offline). Falsy values (`0`, `false`, `no`, `off`, blank; case-insensitive) do **not** enable mock mode |
| `BRIGID_SINCE` | — | Default git ref for `--since` (CLI flag overrides) |
| `BRIGID_PLUGIN_DIRS` | — | Comma-separated plugin directories for custom kind detectors |

---

## Commands

### `brigid generate`

The full pipeline: crawl → identify → relationships → order → chapters →
setup → overview → combine. Produces a multi-chapter Markdown + Mermaid
tutorial.

```bash
brigid generate --dir PATH [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--dir` | (required) | Source directory to analyze |
| `--output-dir` | `./output` | Where to write the tutorial |
| `--checkpoint-dir` | temp under output | Checkpoint directory for resume |
| `--language` | `en` | Tutorial locale (`en` or `es`) |
| `--diagram-level` | `auto` | Mermaid diagram density (`off`, `auto`, `verbose`) |
| `--apps` | — | Scope to specific monorepo apps (comma-separated paths) |
| `--each-app` | off | Generate one tutorial per app in a monorepo |
| `--review-chapters` | off | Second LLM pass to polish each chapter |
| `--concurrency` | 4 | Parallel LLM calls during map/reduce stages |
| `--max-llm-calls` | from config | Hard cap on total LLM calls (fail-closed) |
| `--max-abstractions` | from config | Cap on identified modules |
| `--single-shot` | off | One-shot identify instead of map/reduce |
| `--since` | — | Git ref: only crawl files changed since this ref |
| `--force-setup` | off | Always generate setup guide |
| `--no-setup` | off | Skip setup guide |
| `--no-overview` | off | Skip architecture overview |
| `--format` | `text` | Output format (`text` or `json`) |
| `--verbose` / `-v` | off | Verbose logging |
| `--quiet` / `-q` | off | Suppress non-error output |

### Per-stage subcommands

Each pipeline stage is available as a standalone subcommand for debugging:

| Command | What it does |
|---------|-------------|
| `brigid identify` | Map/reduce abstraction identification |
| `brigid relationships` | Analyze inter-module relationships |
| `brigid order` | Compute chapter ordering |
| `brigid chapters` | Write chapter content + diagrams |
| `brigid setup` | Generate setup guide |
| `brigid overview` | Generate architecture overview |
| `brigid combine` | Assemble final tutorial index |

All accept `--dir`, `--checkpoint-dir`, `--format`, and `--config`.

### Utility commands

| Command | What it does |
|---------|-------------|
| `brigid crawl --dir PATH` | Local file inventory (zero LLM) |
| `brigid dry-run --dir PATH` | Crawl + scope + setup assessment + budget (zero LLM) |
| `brigid eval --out PATH` | Structural tutorial quality gate (zero LLM) |
| `brigid init [--check]` | Write or validate a starter `brigid.toml` |
| `brigid resume --checkpoint PATH` | Report next/pending stages from a checkpoint |
| `brigid completions --shell SHELL` | Generate shell completion scripts |
| `brigid manpage` | Generate a troff-formatted man page |

---

## Examples

```bash
# Inventory a repo
brigid crawl --dir tests/fixtures/python-lib --format json

# Dry-run plan (optionally scope monorepo apps)
brigid dry-run --dir tests/fixtures/umbrella --apps apps/alpha

# Structural eval of a tutorial tree
brigid eval --out tests/fixtures/tutorials/good-mini

# Config + checkpoint status
brigid init --dir /tmp/brigid-demo
brigid resume --checkpoint /tmp/brigid-demo --format json

# Full generate pipeline
brigid generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorial --language en

# Generate per-app tutorials in a monorepo
brigid generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorials --each-app

# Generate with Spanish chrome and chapter review
brigid generate --dir tests/fixtures/umbrella \
  --output-dir /tmp/tutorial --language es --review-chapters

# Incremental: only re-explain modules changed since a release tag
brigid generate --dir . --since v1.2.0 --output-dir /tmp/tutorial

# Run a single stage for debugging
brigid relationships --dir tests/fixtures/umbrella \
  --checkpoint-dir /tmp/checkpoint
```

---

## Performance tips

Large monorepo runs can take dozens of LLM calls. These knobs keep runs fast
and cheap:

- **Disk cache (enabled by default)** — LLM responses are cached on disk
  keyed by `hash(prompt)+model+provider`, so re-runs with an unchanged prompt
  are free. To bypass the cache for a single run (e.g. after changing a
  prompt template), set `BRIGID_NO_CACHE=1`. See
  [ADR 0009](adr/0009-disk-cache-default-lru-eviction.md).
- **Concurrency tuning (`--concurrency`)** — Controls how many LLM calls run
  in parallel during the map/reduce stages.
  - `--concurrency 8` is a good default for **local LLMs** (Ollama, LM Studio)
    where you are not paying per call and the bottleneck is local throughput.
  - `--concurrency 4` is safer for **cloud providers** (DeepSeek, OpenAI) to
    stay under rate limits. Raise it only if your provider tier allows it.
- **Incremental runs (`--since <git-ref>`)** — Only crawl files that changed
  since a tag, commit, or branch (ADR 0013). This skips the full tree walk
  and limits LLM work to changed modules — huge for CI and editor
  integrations on large repos. Requires `git` on `PATH`.
- **Scope with `--apps`** — In monorepos, scope to a single app to cut the
  file corpus and LLM call count dramatically.

---

## Shell completions

`brigid` can generate completion scripts for bash, zsh, fish, and PowerShell.
The scripts cover every subcommand and flag automatically.

```bash
# Print a script to stdout
brigid completions --shell bash
brigid completions --shell zsh
brigid completions --shell fish
brigid completions --shell powershell

# Write directly to a file
brigid completions --shell bash --output ~/.brigid-completions.bash
```

### Installation per shell

**bash** — install into the system completions directory (or source it from
your `.bashrc`):

```bash
brigid completions --shell bash --output /etc/bash_completion.d/brigid
# or, for a user-level install:
brigid completions --shell bash --output ~/.local/share/bash-completion/completions/brigid
```

**zsh** — place the script on your `$fpath` (commonly `~/.zsh/completions`):

```bash
mkdir -p ~/.zsh/completions
brigid completions --shell zsh --output ~/.zsh/completions/_brigid
```

Make sure `~/.zsh/completions` is on your `fpath` (add
`fpath=(~/.zsh/completions $fpath)` to `~/.zshrc`) and run `compinit`.

**fish** — drop the script into the fish completions directory:

```bash
mkdir -p ~/.config/fish/completions
brigid completions --shell fish --output ~/.config/fish/completions/brigid.fish
```

**PowerShell** — add the script to your PowerShell profile:

```powershell
brigid completions --shell powershell --output $PROFILE
# or append to an existing profile:
brigid completions --shell powershell | Add-Content $PROFILE
```

Reload your shell (or open a new terminal) after installing.

---

## Man page

`brigid` can generate a troff-formatted man page covering every subcommand and
flag. The man page includes SYNOPSIS, DESCRIPTION, OPTIONS, COMMANDS,
EXAMPLES, ENVIRONMENT, FILES, EXIT STATUS, and SEE ALSO sections.

```bash
# Print the man page to stdout
brigid manpage

# Write it to a file and install it
brigid manpage > brigid.1 && sudo mv brigid.1 /usr/local/share/man/man1/

# Or write directly to a file with --output
brigid manpage --output brigid.1
man -l brigid.1
```

---

## Configuration file (`brigid.toml`)

`brigid init` writes a starter `brigid.toml`. Configuration precedence is
**CLI > file > env > defaults**.

```toml
# Example brigid.toml
max_llm_calls = 200
max_abstractions = 30

[plugins]
dirs = ["./plugins"]

[[allowed_hosts]]
host = "my-proxy.internal"
```

Run `brigid init --check` to validate an existing `brigid.toml` without
writing anything.

---

## JSON output

Every pipeline stage supports `--format json`, emitting a versioned
`StageOutput<T>` envelope:

```json
{
  "schema_version": 1,
  "stage": "identify",
  "status": "ok",
  "data": { ... },
  "stats": { "llm_calls": 12, "duration_ms": 4500 }
}
```

This is stable across releases (ADR 0012) and suitable for CI integration
and editor plugins.
