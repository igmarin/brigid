# ADR 0010: Release Strategy (GitHub Releases / Homebrew / cargo install)

## Status

Accepted — revised 2026-07-27 (simplified to Linux-only pre-built binary).

## Date

2026-07-25

## Context

M5 (#209) introduced a release workflow to distribute `brigid` as pre-built
binaries. Before this, the only installation method was building from source
via `cargo build`. The product needs multiple distribution channels to reach
different user audiences:

- **Rust developers** — already have `cargo`, prefer `cargo install`.
- **macOS users** — expect `brew install`.
- **CI / automation users** — want a direct binary download with checksum
  verification.

### Constraints

- The release workflow must produce a Linux x86_64 binary with SHA-256
  checksum verification.
- The release archive must include the `brigid` binary, the man page, and
  shell completion scripts (bash, zsh, fish, PowerShell).
- macOS users install via Homebrew (which compiles from source) or
  `cargo install` (which also compiles from source).
- Windows users install via `cargo install` (compiles from source).
- The workflow must be triggerable both by tag push (`vX.Y.Z`) and manually
  (with a dry-run option for validation without publishing).
- Release notes should be extracted from `CHANGELOG.md` automatically.

### Revision history

The original strategy (2026-07-25) shipped pre-built binaries for 5 targets:
Linux x86_64, Linux aarch64, macOS x86_64, macOS aarch64, and Windows x86_64.
This was revised on 2026-07-27 to ship only Linux x86_64 after observing that:

- macOS Intel runners (`macos-13`) are being phased out by GitHub and have
  long queue times (26+ minutes waiting for a runner).
- macOS runners cost 10x more than Linux runners on GitHub Actions.
- macOS users already have two good options: Homebrew (compiles natively)
  and `cargo install` (compiles natively).
- Windows users typically have `cargo` installed for Rust CLI tools.
- `cargo-binstall` falls back to compiling from source when no matching
  pre-built binary exists, so it still works on macOS/Windows.
- The 5-target matrix made the release workflow 20+ minutes long; the
  Linux-only approach completes in ~3 minutes.

## Decision

Adopt a **simplified multi-channel release strategy** with a single
pre-built Linux binary and source-based installation for other platforms:

### 1. GitHub Releases (canonical — Linux x86_64 only)

A GitHub Actions workflow (`.github/workflows/release.yml`) triggers on tag
push (`v*.*.*`) or manual dispatch:

1. **Build** — builds `brigid` natively on Ubuntu, strips the binary, then
   runs `brigid manpage` and `brigid completions --shell {bash,zsh,fish,powershell}`
   to generate the man page and completion scripts.
2. **Packaging** — a single `.tar.gz` archive containing the binary + man
   page + completions + README.
3. **Checksums** — a `.sha256` file for the archive.
4. **GitHub Release** — created with notes extracted from `CHANGELOG.md`.
   The dry-run mode validates the build without publishing.

### 2. Homebrew (macOS — source build)

A Homebrew formula template lives in `homebrew/brigid.rb`. The canonical live
formula is maintained in the `igmarin/homebrew-tap` repository. The formula:

- Downloads the source tarball from the GitHub Release tag.
- Builds from source using `cargo install --locked`.
- Installs the binary automatically (man page and completions are generated
  by the binary at runtime via `brigid manpage` / `brigid completions`).

Users install via:
```bash
brew tap igmarin/homebrew-tap
brew install brigid
```

### 3. cargo install (all platforms)

`brigid-cli` is published to crates.io, so users can install from source:
```bash
cargo install brigid-cli
```
Or directly from the git repository:
```bash
cargo install --git https://github.com/igmarin/brigid brigid-cli
```

### 4. cargo-binstall

`brigid-cli` includes `[package.metadata.binstall]` configuration so that
`cargo binstall brigid-cli` downloads the pre-built Linux binary from GitHub
Releases. On macOS/Windows, `cargo-binstall` falls back to compiling from
source.

### 5. Direct download (Linux)

Linux users can download the `.tar.gz` archive directly from the GitHub
Releases page, verify the checksum, and install manually. The README
documents this flow.

## Alternatives Considered

### Multi-platform pre-built binaries (original strategy)

- **Pros**: Fast install for macOS/Windows users without a Rust toolchain.
- **Cons**: macOS runners are slow and expensive (10x Linux cost); Intel Mac
  runners are being deprecated; 5-target matrix takes 20+ minutes; Windows
  cross-compilation adds complexity.
- **Rejected**: The cost (CI time, runner fees, maintenance) outweighs the
  benefit when macOS users have Homebrew and `cargo install` available.

### cargo-dist only

- **Pros**: Automated, well-maintained, handles cross-compilation and
  checksums.
- **Cons**: Adds a build dependency and configuration layer. The custom
  workflow gives full control over packaging and is already working.
- **Rejected**: The custom workflow is sufficient and more transparent.

### Snap / Flatpak / AUR

- **Pros**: Native package manager integration on Linux.
- **Cons**: Significant per-distribution maintenance overhead. The `.tar.gz`
  archive + direct download covers Linux adequately for a CLI tool.
- **Rejected**: Not worth the maintenance cost at this stage. Can be added
  later if there is user demand.

## Consequences

- **Positive**: Release workflow completes in ~3 minutes (down from 20+).
- **Positive**: No dependency on macOS/Windows runners — cheaper and more
  reliable CI.
- **Positive**: macOS users get native compilation via Homebrew or cargo,
  which produces optimized binaries for their specific architecture.
- **Positive**: SHA-256 checksums enable supply-chain verification for the
  Linux binary.
- **Positive**: The dry-run mode lets maintainers validate the release build
  before publishing.
- **Negative**: macOS/Windows users without a Rust toolchain cannot use a
  pre-built binary. They must install Rust first (or use Homebrew, which
  handles the Rust dependency automatically).
- **Negative**: The Homebrew formula must be updated manually for each
  release — the `sha256` value changes per version.

## Related Documents

- `.github/workflows/release.yml` — the release workflow.
- `homebrew/brigid.rb` — Homebrew formula template (source build).
- `crates/brigid-cli/Cargo.toml` — `[package.metadata.binstall]` configuration.
- [`CHANGELOG.md`](../../CHANGELOG.md) — release notes source.
- [`README.md`](../../README.md) — installation instructions for all channels.
- Issue #209 — release workflow with native installers and Homebrew formula.
