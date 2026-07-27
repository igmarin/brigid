# ADR 0012: JSON Output Schema for Pipeline Stages

## Status

Accepted

## Date

2026-07-28

## Context

The `brigid` CLI needs machine-readable JSON output for integration with CI
pipelines, editor plugins, and programmatic consumers. Prior to this ADR,
only a handful of subcommands (`crawl`, `dry-run`, `eval`, `resume`)
supported `--format json`, each with an ad-hoc JSON shape. The pipeline
stages (`identify`, `relationships`, `order`, `chapters`, `setup`,
`overview`, `combine`, `generate`) had no JSON output at all.

The core challenge is **schema stability**: once external tools depend on a
JSON shape, breaking changes (renamed fields, removed fields, changed types)
cause silent failures. We need a design that:

1. Provides a consistent, predictable JSON envelope across all stages.
2. Makes the schema version explicit so consumers can detect breaking changes.
3. Prevents accidental schema drift through automated tests.
4. Documents the stability guarantees and evolution policy.

## Decision

### 1. `StageOutput<T>` Envelope

All JSON output uses a generic envelope type defined in
`brigid-core::stage_output`:

```rust
pub struct StageOutput<T> {
    pub schema_version: u32,
    pub stage: String,
    pub status: StageStatus,  // "ok" | "error"
    pub data: T,
    pub stats: Option<StageStats>,  // omitted when None
}
```

Every stage's JSON output shares this envelope. The `data` field carries
the stage-specific payload (e.g., `IdentifyOutput`, `GenerateOutput`). The
`stats` field is optional and omitted from serialization when `None` (via
`skip_serializing_if`).

### 2. `schema_version` Field

The envelope includes a `schema_version` field (currently `1`), exported as
`SCHEMA_VERSION` from `brigid-core::stage_output`. This version number applies
to the **envelope structure** and the **set of stage output types**. It does
not version individual stage payloads independently.

**Stability guarantee:** The `schema_version` is bumped on any breaking
change to the JSON schema. Breaking changes include:

- Removing a field from any output struct.
- Renaming a field (JSON key change).
- Changing a field's type (e.g., `u32` → `String`).
- Changing the envelope structure (adding required fields, removing
  optional fields that consumers may expect).

**Non-breaking changes** (no version bump required):

- Adding a new optional field (serialized with a default value or
  `skip_serializing_if`).
- Adding a new stage output type.
- Adding a new variant to `StageStatus` (consumers should handle unknown
  variants gracefully).

### 3. Snake_case Field Naming

All JSON field names use `snake_case`, enforced by
`#[serde(rename_all = "snake_case")]` on every output struct. This is
idiomatic for JSON APIs and consistent with the Rust field names.

### 4. Schema Stability Tests

Frozen JSON snapshots live in `tests/fixtures/json-schemas/`, one file per
stage output type:

- `identify.json`
- `relationships.json`
- `order.json`
- `chapters.json`
- `setup.json`
- `overview.json`
- `combine.json`
- `generate.json`

Tests in `brigid-core::stage_output::tests` (prefixed `schema_stability_*`)
construct sample data, serialize it with `serde_json`, and compare against
the frozen snapshot using `assert-json-diff::assert_json_eq!`. If a
developer accidentally changes a field name or type, the test fails and
points to the exact path of the mismatch.

**When updating a snapshot:** If a breaking change is intentional, bump
`SCHEMA_VERSION`, update the frozen JSON file, and document the change in
the commit message and release notes.

### 5. `GenerateOutput` for Full Pipeline

The `generate` subcommand's JSON output uses `GenerateOutput`:

```rust
pub struct GenerateOutput {
    pub stages: Vec<StageSummary>,
    pub output_dir: String,
    pub checkpoint_path: String,
    pub total_llm_calls: u32,
    pub elapsed_ms: u64,
}

pub struct StageSummary {
    pub name: String,
    pub status: String,       // "ok" or "error"
    pub duration_ms: u64,
    pub llm_calls: u32,
}
```

This provides a high-level summary of the entire pipeline run — which stages
ran, how long each took, how many LLM calls each consumed, and where the
output and checkpoint were written. It does **not** include the full stage
payloads (abstractions, chapters, etc.); consumers who need those should run
the per-stage subcommands with `--format json`.

## Alternatives Considered

### Option A — Per-stage ad-hoc JSON (no envelope)

Each stage emits its own JSON shape with no shared wrapper.

- **Pros**: Minimal boilerplate; each stage's JSON is as small as possible.
- **Cons**: No consistent metadata (schema version, status); consumers must
  handle each stage's JSON differently; no central place to enforce
  stability.
- **Rejected**: The envelope provides consistency and forward compatibility
  with negligible overhead.

### Option B — JSON Schema (draft 2020-12) files instead of frozen snapshots

Generate formal JSON Schema files and validate output against them.

- **Pros**: Machine-readable schema; can be published for code generation.
- **Cons**: Adds a build dependency and validation step; schemas are harder
  to maintain than Rust structs; the frozen snapshot approach catches
  accidental drift more directly.
- **Rejected**: Can be added later as a layer on top of the frozen snapshots
  if external consumers request a formal schema document.

### Option C — Versioned endpoints (`/v1/`, `/v2/`)

Embed the schema version in the CLI flag (e.g., `--format json:v1`).

- **Pros**: Multiple schema versions can coexist.
- **Cons**: Adds CLI complexity; the CLI is a single binary, not an HTTP API;
  the `schema_version` field in the output is sufficient for consumers to
  detect version mismatches.
- **Rejected**: The `schema_version` field is simpler and sufficient for a
  CLI tool.

## Consequences

- **Positive**: All JSON output shares a consistent, versioned envelope.
  Consumers can parse the `schema_version` and `stage` fields before
  dispatching to stage-specific logic.
- **Positive**: Schema stability tests catch accidental breaking changes at
  CI time, before they reach users.
- **Positive**: The `StageOutput::new` constructor ensures `schema_version`
  is always set correctly — developers cannot forget to include it.
- **Negative**: Adding a new required field to any output struct requires a
  `SCHEMA_VERSION` bump and snapshot update. This is intentional friction.
- **Negative**: The frozen snapshots must be updated when non-breaking
  additions are made (new optional fields). This is a minor maintenance cost.

## Related Documents

- [`crates/brigid-core/src/stage_output.rs`](../../crates/brigid-core/src/stage_output.rs) —
  the `StageOutput<T>` envelope and all stage output types.
- [`tests/fixtures/json-schemas/`](../../tests/fixtures/json-schemas/) —
  frozen JSON snapshots for schema stability tests.
- ADR 0001 — Checkpoint schema v1 (the checkpoint format that `generate`
  produces).
- Issue #223 — Add `--format json` to `brigid generate` + JSON schema
  stability tests + ADR 0012.
