# ADR 0005: Migrate YAML Parser to serde_yaml_ng

## Status

Accepted

## Date

2026-07-24

## Context

[ADR 0004](0004-yaml-parser-migration.md) migrated the workspace off the unsound
`serde_yml`/`libyml` crates (RUSTSEC-2025-0067/0068) to `serde_yaml` 0.9 (the
`dtolnay/serde-yaml` crate). That resolved the security issue. However,
`serde_yaml` 0.9 is marked **deprecated** upstream — dtolnay's repository is
archived and no further releases are expected. The crate still depends on
`unsafe-libyaml` (maintained), and no RustSec advisory has been filed against it,
so this is **low-priority tech debt, not a security issue**.

Issue #114 tracks the deprecation and asks the project to evaluate maintained
alternatives and migrate when a suitable drop-in is available.

### Candidates evaluated

| Crate | Latest | Verdict |
|-------|--------|---------|
| **`serde_yaml_ng`** | 0.10.0 (2024-05-26) | **Chosen** — drop-in fork of `serde_yaml` with identical API (`from_str`, `to_string`, `Error`). 6.4 M downloads, 572 reverse-dependents, MSRV 1.64. |
| `serde_yml` | 0.0.13 (2026-05-27) | **Rejected** — now also deprecated. The 0.0.13 release is a thin compatibility shim forwarding to `noyalib`. Migrating here would simply track another deprecation. |
| `noyalib` | 0.0.15 (2026-07-12) | **Rejected** — pure-Rust, `#![forbid(unsafe_code)]`, but still 0.0.x with ~108 K downloads and MSRV 1.85. Too new for a low-priority tech-debt migration; revisit once it stabilises past 0.0. |
| `serde-saphyr` | 0.0.x | **Rejected** — no `Value` DOM and no `to_string` serializer. `brigid-pipeline` serialises candidate abstractions to YAML (`candidates_to_yaml`), so a serializer is required. |
| `yaml-rust2` | 0.9 | **Rejected** — lower-level parser, not serde-integrated. Would require a hand-written serde adapter. |

### Constraints preserved from ADR 0004

- The secret-field guard (issue #73) must remain: YAML is parsed into
  `serde_json::Value`, scanned for secret-bearing keys, then deserialised into
  `RunConfig`.
- `brigid-pipeline` uses both `from_str` (typed deserialisation) and `to_string`
  (serialisation), so the replacement crate must support both directions.
- Error messages are asserted on variants/emptiness, not exact text (ADR 0004
  Consequences), so a parser swap with different error strings is safe.

## Decision

Replace `serde_yaml` 0.9 with `serde_yaml_ng` 0.10 in both `brigid-core` and
`brigid-pipeline`.

### Dependency change

`crates/brigid-core/Cargo.toml` and `crates/brigid-pipeline/Cargo.toml`:

```toml
serde_yaml_ng = "0.10"
```

(replacing `serde_yaml = "0.9"`)

### Code change

All `serde_yaml::` path references are replaced with `serde_yaml_ng::`. Because
`serde_yaml_ng` is a fork from the final commit of `serde_yaml`, the API is
identical — `from_str`, `to_string`, and `Error` are drop-in replacements.

`crates/brigid-core/src/config.rs`, `parse_yaml_config`:

```rust
pub fn parse_yaml_config(text: &str) -> Result<RunConfig, ConfigError> {
    let value: serde_json::Value =
        serde_yaml_ng::from_str(text).map_err(|e| ConfigError::Yaml(e.to_string()))?;
    check_for_secret_fields(&value)?;
    serde_json::from_value(value).map_err(|e| ConfigError::Yaml(e.to_string()))
}
```

`crates/brigid-pipeline/src/identify.rs`:

- `IdentifyError::Parse(#[from] serde_yaml_ng::Error)`
- `serde_yaml_ng::from_str` (three call sites)
- `serde_yaml_ng::to_string` (one call site)

The parse-to-`serde_json::Value`-first flow from issue #73 is preserved
unchanged.

### `deny.toml`

No change required. `ignore = []` remains — no advisories are ignored.
`serde_yaml_ng` has no RustSec advisories.

## Alternatives Considered

### Keep `serde_yaml` 0.9 and only document the deprecation

- **Pros**: Zero code change; the crate works and has no advisory.
- **Cons**: Leaves a deprecated, archived dependency in the supply chain. The
  project's review prompt asks whether dependencies are free of known
  vulnerabilities; while not a vulnerability, a deprecated parser is a
  maintenance liability that rs-guard and future audits may flag.
- **Rejected**: `serde_yaml_ng` is a zero-risk drop-in, so there is no reason to
  defer.

### Migrate to `serde_yml` 0.0.13 (the issue's initial suggestion)

- **Pros**: Was previously considered the "maintained fork".
- **Cons**: `serde_yml` is now itself deprecated (0.0.13 is a shim to `noyalib`).
  Migrating to a deprecated crate to escape a deprecated crate is
  counter-productive.
- **Rejected**: See candidate table above.

### Migrate to `noyalib`

- **Pros**: Pure Rust, `#![forbid(unsafe_code)]`, YAML 1.2 compliant.
- **Cons**: Still 0.0.x, ~108 K downloads, MSRV 1.85. Introducing a very young
  crate for a low-priority tech-debt item adds more risk than it removes.
- **Rejected**: Revisit once `noyalib` stabilises past 0.0.

## Consequences

- **Positive**: The workspace no longer depends on a deprecated, archived crate.
  `cargo deny` and `cargo audit` pass with zero ignored advisories.
- **Positive**: The secret-field guard flow is preserved exactly.
- **Positive**: All existing YAML config and pipeline tests pass without
  modification — `serde_yaml_ng` is a behaviour-preserving drop-in.
- **Negative**: `serde_yaml_ng` is maintained by a single maintainer (acatton).
  If it too becomes unmaintained, the project should evaluate `noyalib` (by then
  hopefully stabilised) or another maintained parser. This remains a future
  tech-debt item, not a security blocker.
- **Negative**: `serde_yaml_ng` and `serde_yaml` have subtly different error
  messages. No test asserts on exact error text, so this is a non-issue today,
  but future tests should continue to assert on error variants, not exact
  strings, to stay parser-agnostic.

## Related Documents

- [ADR 0004](0004-yaml-parser-migration.md) — the prior migration off
  `serde_yml`/`libyml` to `serde_yaml` 0.9. Superseded by this ADR.
- `crates/brigid-core/src/config.rs` — `parse_yaml_config` and
  `check_for_secret_fields`.
- `crates/brigid-pipeline/src/identify.rs` — YAML parse/serialise call sites.
- `crates/brigid-core/Cargo.toml`, `crates/brigid-pipeline/Cargo.toml` —
  `serde_yaml_ng = "0.10"` dependency.
- `deny.toml` — `ignore = []`.
- Issue #114 — tracks this migration.
