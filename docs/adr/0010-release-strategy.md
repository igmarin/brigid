# ADR 0010: Release Strategy (GitHub Releases / Homebrew / cargo install)

## Status

Accepted

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
- **Windows users** — need a native binary, not a Unix-only build.

### Constraints

- The release workflow must produce binaries for Linux (x86_64, aarch64),
  macOS (x86_64, aarch64), and Windows (x86_64).
- Each release archive must include the `brigid` binary, the man page, and
  shell completion scripts (bash, zsh, fish, PowerShell).
- SHA-256 checksums must be published for verification.
- The Homebrew formula must support both Intel and Apple Silicon Macs.
- The workflow must be triggerable both by tag push (`vX.Y.Z`) and manually
  (with a dry-run option for validation without publishing).
- Release notes should be extracted from `CHANGELOG.md` automatically.

## Decision

Adopt a **multi-channel release strategy** centered on GitHub Releases as the
canonical artifact source, with Homebrew and cargo install as additional
channels:

### 1. GitHub Releases (canonical)

A GitHub Actions workflow (`.github/workflows/release.yml`) triggers on tag
push (`v*.*.*`) or manual dispatch:

1. **Docs job** — builds `brigid` natively on Ubuntu, runs `brigid manpage` and
   `brigid completions --shell {bash,zsh,fish,powershell}` to generate the man
   page and completion scripts. These are platform-independent and bundled
   into every archive.
2. **Build matrix** — one job per `(OS, target)` pair:
   - `x86_64-unknown-linux-gnu` (Ubuntu, native)
   - `aarch64-unknown-linux-gnu` (Ubuntu, cross)
   - `x86_64-apple-darwin` (macOS 13, native)
   - `aarch64-apple-darwin` (macOS 14, native)
   - `x86_64-pc-windows-msvc` (Windows, native)
3. **Packaging** — each archive contains the binary + man page + completions,
   compressed as `.tar.gz` (Unix) or `.zip` (Windows).
4. **Checksums** — a `SHA256SUMS` file listing the SHA-256 hash of every
   archive.
5. **GitHub Release** — created with auto-generated notes extracted from
   `CHANGELOG.md`. The dry-run mode validates the build without publishing.

### 2. Homebrew (macOS)

A Homebrew formula template lives in `homebrew/brigid.rb`. The canonical live
formula is maintained in the `igmarin/homebrew-tap` repository. The formula:

- Supports both Intel (`on_intel`) and Apple Silicon (`on_arm`) Macs via
  `on_macos` blocks with separate URLs and SHA-256 hashes.
- Installs the binary, man page, and completions automatically.

Users install via:
```bash
brew tap igmarin/homebrew-tap
brew install brigid
```

### 3. cargo install

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
`cargo binstall brigid-cli` downloads a pre-built binary from GitHub Releases
instead of compiling from source. This bridges the `cargo install` and direct
download channels.

### 5. Direct download

Users can download archives directly from the GitHub Releases page, verify
checksums with `sha256sum -c SHA256SUMS`, and install manually. The README
documents this flow including man page and completion installation.

## Alternatives Considered

### cargo-dist only

- **Pros**: Automated, well-maintained, handles cross-compilation and
  checksums.
- **Cons**: Adds a build dependency and configuration layer. The custom
  workflow gives full control over packaging (man page + completions in every
  archive) and is already working.
- **Rejected**: The custom workflow is sufficient and more transparent.

### Single-channel (GitHub Releases only)

- **Pros**: Simplest to maintain — one artifact source.
- **Cons**: macOS users expect `brew install`; Rust developers expect
  `cargo install`. A single channel limits adoption.
- **Rejected**: Multi-channel maximizes reach with minimal additional
  maintenance (the Homebrew formula is a template updated per release; cargo
  install requires only crates.io publishing).

### Snap / Flatpak / AUR

- **Pros**: Native package manager integration on Linux.
- **Cons**: Significant per-distribution maintenance overhead. The `.tar.gz`
  archive + direct download covers Linux adequately for a CLI tool.
- **Rejected**: Not worth the maintenance cost at this stage. Can be added
  later if there is user demand.

## Consequences

- **Positive**: Users on all three major platforms (Linux, macOS, Windows)
  can install `brigid` in under two minutes via their preferred channel.
- **Positive**: SHA-256 checksums enable supply-chain verification.
- **Positive**: The dry-run mode lets maintainers validate the release build
  before publishing.
- **Negative**: Five build targets means five CI jobs per release, increasing
  workflow runtime. Cross-compilation for `aarch64-unknown-linux-gnu` requires
  `cross` (Docker), adding a dependency.
- **Negative**: The Homebrew formula must be updated manually (or via a
  script) for each release — the `url` and `sha256` values change per version.
  This is a deliberate human action, not fully automated.
- **Negative**: crates.io publishing is a separate manual step from the GitHub
  Release workflow.

## Related Documents

- `.github/workflows/release.yml` — the release workflow.
- `homebrew/brigid.rb` — Homebrew formula template.
- `crates/brigid-cli/Cargo.toml` — `[package.metadata.binstall]` configuration.
- [`CHANGELOG.md`](../../CHANGELOG.md) — release notes source.
- [`README.md`](../../README.md) — installation instructions for all channels.
- Issue #209 — release workflow with native installers and Homebrew formula.
