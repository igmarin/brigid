# ADR 0004: YAML Parser Migration off serde_yml/libyml

## Status

Superseded by [ADR 0005](0005-yaml-parser-serde-yaml-ng.md) — `serde_yaml` 0.9 is
deprecated upstream; the workspace migrated to the maintained `serde_yaml_ng`
fork in issue #114.

## Date

2026-07-24

## Context

`brigid` reads layered configuration from `brigid.toml` (TOML) and `.brigid.yaml`
(YAML), plus YAML/JSON blocks extracted from messy LLM output. The YAML parsing
path lives in `crates/brigid-core/src/config.rs` (`parse_yaml_config`).

The project originally used `serde_yml` 0.0.12 for YAML deserialization.
`serde_yml` transitively depends on `libyml` 0.0.5. Both crates were flagged by
RustSec:

- **RUSTSEC-2025-0067** — `libyml` is unmaintained / unsound.
- **RUSTSEC-2025-0068** — `serde_yml` is unmaintained / unsound.

`cargo deny` and `cargo audit` are CI gates (see `.github/workflows/ci.yml`
and `deny.toml`). While the advisories were temporarily ignored in `deny.toml`
to keep CI green, shipping a security-sensitive CLI — one that crawls arbitrary
repository contents, transmits `Authorization` headers, and can trigger many
paid LLM calls — on top of known-unsound parsing crates is unacceptable per
the project's own review standard (`.github/review-prompt.md` §2: "Are
dependencies free of known vulnerabilities (`cargo deny` / `cargo audit` in
CI)?").

The migration was tracked in issue #75 and landed in commit `56f829d`
("Migrate brigid-core off unsound serde_yml/libyml (#75)").

### Additional constraint: the secret-field guard

Issue #73 introduced a defense-in-depth secret-field guard: YAML is first
parsed into a `serde_json::Value`, then `check_for_secret_fields` scans the
value tree for secret-bearing keys **before** deserializing into `RunConfig`.
The migration must preserve this parse-to-Value-first flow so that unknown
secret-like keys are still rejected.

## Decision

Replace `serde_yml` with `serde_yaml` 0.9 (the `dtolnay/serde-yaml` crate) in
`brigid-core`'s YAML parsing path.

### Dependency change

`crates/brigid-core/Cargo.toml`:

```toml
serde_yaml = "0.9"
```

(replacing `serde_yml = "0.0.12"`)

### Code change

`crates/brigid-core/src/config.rs`, `parse_yaml_config`:

```rust
pub fn parse_yaml_config(text: &str) -> Result<RunConfig, ConfigError> {
    let value: serde_json::Value =
        serde_yaml::from_str(text).map_err(|e| ConfigError::Yaml(e.to_string()))?;
    check_for_secret_fields(&value)?;
    serde_json::from_value(value).map_err(|e| ConfigError::Yaml(e.to_string()))
}
```

The only change is `serde_yml::from_str` → `serde_yaml::from_str`. The
parse-to-`serde_json::Value`-first flow introduced by #73 is preserved
unchanged: `serde_yaml` deserializes into `serde_json::Value`, the secret-field
guard runs, and the value is then deserialized into `RunConfig`.

### `deny.toml` change

The `RUSTSEC-2025-0067` and `RUSTSEC-2025-0068` entries were removed from the
`[advisories] ignore` list, and `ignore = []` — no advisories are ignored. The
comment in `deny.toml` records this:

```toml
[advisories]
# serde_yml/libyml (RUSTSEC-2025-0067/0068) were removed in #75; no
# advisories are currently ignored.
ignore = []
```

### Verification

- All existing YAML config tests (`parse_toml_and_yaml_layers`,
  `invalid_yaml_errors`, `yaml_*_rejected`) pass unchanged — the
  `serde_yaml::from_str` API is a drop-in replacement for the
  `serde_yml::from_str` call in this code path.
- `cargo audit`: 0 warnings.
- `cargo deny check`: passes.

## Alternatives Considered

### Keep `serde_yml` and continue ignoring the advisories

- **Pros**: No code change.
- **Cons**: The project's CI gates (`cargo deny`, `cargo audit`) would
  permanently carry ignored advisories, weakening the supply-chain posture of a
  security-sensitive CLI. The review prompt explicitly treats vulnerable
  dependencies as a blocking finding. `serde_yml` and `libyml` are unmaintained,
  so no upstream fix is forthcoming.
- **Rejected**: Violates the project's security standard and leaves a known
  unsoundness in the parsing path that handles user-supplied config files.

### `serde_yaml_ng` (community fork)

- **Pros**: Actively maintained fork of `serde_yaml` that continues the
  original work.
- **Cons**: At the time of the #75 migration, `serde_yaml` 0.9 (dtolnay) was
  sufficient, widely deployed, and met all requirements. Introducing a newer
  fork would add a less-audited dependency for no functional gain. The fork
  can be adopted later if `serde_yaml` itself becomes unmaintained.
- **Rejected**: Not needed for the current migration; tracked as a future
  option (see Consequences).

### Hand-written YAML parser

- **Pros**: No external YAML dependency.
- **Cons**: YAML is a complex format (anchors, aliases, tags, multi-line
  scalars). A hand-written parser would be a large, error-prone undertaking
  and a security liability in its own right. The config files in this project
  are simple flat/nested mappings, but relying on a subset parser would break
  on valid YAML that users might supply.
- **Rejected**: Disproportionate cost and risk for a config-parsing path.

## Consequences

- **Positive**: `brigid-core` no longer depends on any crate flagged by
  RUSTSEC-2025-0067 or RUSTSEC-2025-0068. `cargo deny` and `cargo audit` pass
  with zero ignored advisories.
- **Positive**: The secret-field guard flow (parse to `serde_json::Value`,
  scan, then deserialize to `RunConfig`) is preserved exactly, so the
  defense-in-depth behavior from #73 is unchanged.
- **Positive**: All existing YAML config tests pass without modification,
  confirming the migration is behavior-preserving for the config shapes this
  project supports.
- **Negative**: `serde_yaml` 0.9 (dtolnay) is itself in maintenance mode and
  may eventually be deprecated. The project must track its status and migrate
  to a maintained successor (e.g. `serde_yaml_ng`) if a new advisory is filed
  or the crate is yanked. This is a future tech-debt item, not a current
  blocker.
- **Negative**: `serde_yaml` and `serde_yml` have subtly different error
  messages, so any test that asserted on the exact text of a `ConfigError::Yaml`
  message would need updating. No such test existed at migration time, but
  future tests should assert on error variants, not exact message strings, to
  stay parser-agnostic.

## Related Documents

- `docs/adr/0001-checkpoint-schema-v1.md` — checkpoint config is loaded through
  the YAML/TOML config parsing path affected by this migration.
- `crates/brigid-core/src/config.rs` — `parse_yaml_config` and
  `check_for_secret_fields`.
- `crates/brigid-core/Cargo.toml` — `serde_yaml = "0.9"` dependency.
- `deny.toml` — `ignore = []` after removing the RUSTSEC-2025-0067/0068
  entries.
- `.github/review-prompt.md` §2 — the security standard that motivated the
  migration.
- Issue #75 and commit `56f829d` — the migration itself.
