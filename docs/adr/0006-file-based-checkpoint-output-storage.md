# ADR 0006: File-Based Checkpoint Output Storage for M4 Stages

## Status

Accepted

## Date

2026-07-25

## Context

ADR 0001 established the checkpoint format for the pipeline: a small
`checkpoint.json` metadata file plus a compressed `files.ndjson.gz` sidecar for
the crawled file corpus. That design works well for the M1–M3 stages (fetch,
identify), where the "output" of a stage is either the file bundle itself or a
JSON-serialisable list of abstractions stored inline in `checkpoint.json`.

M4 introduces stages whose outputs are **large Markdown documents**: chapter
files (one per abstraction), a setup guide, an architecture overview, and a
combined index. Storing these as inline JSON strings inside `checkpoint.json`
would recreate the original problem ADR 0001 solved for file bodies — the
metadata file grows linearly with output size, becoming an I/O bottleneck on
every stage save/load.

Issue #139 (M4-CHK-1) tracks the need for a durable, resumable output storage
mechanism for these M4 stages.

### Constraints

- Outputs must survive interrupts and be verifiable on resume (no silent
  corruption).
- The existing `checkpoint.json` + `files.ndjson.gz` structure from ADR 0001
  must remain backward-compatible — M1–M3 checkpoints still load.
- Each stage's output is a set of files (e.g. chapters produces N files, one
  per abstraction), not a single blob.
- Resume must be able to detect when a stage is marked complete but its output
  files are missing or corrupt (partial write, disk full, manual deletion).

## Decision

Store M4 stage outputs as **individual files inside the checkpoint directory**,
with metadata (path, SHA-256, size) recorded in a new `stage_outputs` field on
`CheckpointV1`.

### File layout

```
checkpoint-dir/
  checkpoint.json          # metadata + stage_outputs manifest
  files.ndjson.gz          # crawled file corpus (ADR 0001)
  chapters/
    01_intro_to_foo.md
    02_bar_system.md
    ...
  00_setup.md              # setup guide (if generated)
  00_architecture_overview.md  # architecture overview (if generated)
  index.md                 # combined tutorial index
```

### Metadata schema

`CheckpointV1` gains an optional `stage_outputs` field:

```rust
pub struct StageOutputs {
    pub entries: BTreeMap<String, Vec<StageOutputEntry>>,
}

pub struct StageOutputEntry {
    pub path: String,      // relative path inside checkpoint dir
    pub sha256: String,    // "sha256:<hex>" of file bytes
    pub size: u64,         // file size in bytes
}
```

The `stage_outputs` map is keyed by stage name (`"chapters"`, `"setup"`,
`"overview"`, `"combine"`). Each value is a list of `StageOutputEntry`
describing the files written by that stage.

### Integrity verification

Every file write computes `sha256:<hex>` over the raw bytes and records it in
the manifest. On load, `CheckpointStore::read_stage_file` re-computes the
digest and rejects on mismatch (`StageOutputIntegrity` error). A size check
provides a fast-path rejection for truncated files.

`CheckpointStore::is_stage_complete_with_files` extends the existing
`is_stage_complete` check: it verifies that the stage is marked complete in
`completed_stages` **and** that every output file exists on disk with a
matching SHA-256. This catches the case where a stage was marked complete but
the output files were lost (e.g. manual deletion, disk corruption).

### Atomic writes

All stage output files are written atomically: write to a `.tmp` file, then
rename to the final path. This prevents partial writes from being visible on
resume after a crash.

### Backward compatibility

`stage_outputs` is `Option<StageOutputs>` and defaults to `None`. M1–M3
checkpoints without this field load unchanged. The field is additive — no
schema version bump is required.

## Alternatives Considered

### Store all outputs inline in checkpoint.json

- **Pros**: Single file, no extra disk files to manage.
- **Cons**: `checkpoint.json` grows linearly with chapter count and content
  size. Re-parsing the entire JSON on every stage save/load becomes an I/O
  bottleneck for large monorepos. This is the exact problem ADR 0001 avoided
  for file bodies.
- **Rejected**: Defeats the purpose of the content-addressed manifest design.

### Store outputs in a single compressed archive (e.g. outputs.tar.gz)

- **Pros**: One sidecar file, similar to `files.ndjson.gz`.
- **Cons**: Cannot read or update a single chapter without decompressing the
  entire archive. The `--review-chapters` flow updates individual chapters in
  place, so a monolithic archive would require full re-archive on every chapter
  review.
- **Rejected**: Individual files support incremental updates naturally.

### Use a content-addressed blob store (git-style)

- **Pros**: Maximum deduplication; identical chapter content would share a
  blob.
- **Cons**: Over-engineered for this use case. Chapter content is unique per
  abstraction; deduplication gain is negligible. Adds hash-directory
  indirection complexity for no real benefit.
- **Rejected**: Premature optimisation.

## Consequences

- **Positive**: `checkpoint.json` stays small regardless of output volume.
  Stage outputs are readable individually without parsing a large JSON blob.
- **Positive**: SHA-256 verification on every read detects corruption
  immediately, preventing silent resume from damaged state.
- **Positive**: `--review-chapters` can update individual chapter files in
  place without rewriting the entire stage output.
- **Positive**: Backward compatible — M1–M3 checkpoints load unchanged.
- **Negative**: The checkpoint directory now contains more files. Tools that
  expected only `checkpoint.json` + `files.ndjson.gz` must be updated to
  handle the `chapters/` directory and top-level Markdown files.
- **Negative**: Resume from a partially written stage (crash mid-write) may
  find some chapter files present and others missing. The
  `is_stage_complete_with_files` check handles this by returning `false`,
  triggering a full stage re-run. This is safe because stages are idempotent.

## Related Documents

- [ADR 0001](0001-checkpoint-schema-v1.md) — the base checkpoint format this
  ADR extends with file-based stage outputs.
- `crates/brigid-core/src/checkpoint.rs` — `StageOutputEntry`, `StageOutputs`,
  `CheckpointV1::stage_outputs`.
- `crates/brigid-pipeline/src/checkpoint_store.rs` — `write_stage_file`,
  `read_stage_file`, `is_stage_complete_with_files`, `record_stage_outputs`.
- Issue #139 (M4-CHK-1) — tracks this design.
