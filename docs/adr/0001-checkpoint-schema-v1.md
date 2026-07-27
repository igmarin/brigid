# ADR 0001: Checkpoint Schema v1 — Content-Addressed Manifest

## Status

Accepted

## Date

2026-07-23

## Context

`brigid` runs a long, expensive pipeline against code repositories (local trees or
GitHub URLs). A full monorepo tutorial can require dozens of LLM map/reduce
calls, so every expensive stage must be **idempotent and resumable**.

The Python reference implementation stores the entire tutorial generation state
in a single JSON checkpoint file. That works for small repositories, but it
does not scale:

- File bodies are duplicated inside the checkpoint, so storage and memory grow
  **linearly** with total serialized repository contents.
- Oversized checkpoints become an I/O bottleneck because the whole blob must be
  parsed and rewritten after every stage.
- Streaming or partial reads are impractical; resuming loads the entire blob
  into memory even when only a subset of files is needed for the next stage.

We need a durable, versioned checkpoint format that survives interrupts,
supports partial resume, and keeps memory usage bounded as the analyzed
repository grows.

## Decision

Checkpoint v1 is a **small, versioned JSON metadata file** plus a **sidecar file
bundle** for the actual file bodies.

### Metadata file (`checkpoint.json`)

A stable JSON object that contains everything needed to resume the pipeline:

```json
{
  "version": 1,
  "completed_stages": ["fetch", "identify"],
  "stage_timestamps": {
    "fetch": "2026-07-23T14:00:00Z",
    "identify": "2026-07-23T14:05:00Z"
  },
  "config_hash": "sha256:...",
  "config": { "...": "..." },
  "manifest": {
    "path": "files.ndjson.gz",
    "sha256": "...",
    "size": 123456
  },
  "abstractions": [...],
  "relationships": {...},
  "metadata": {
    "created_at": "2026-07-23T14:00:00Z",
    "updated_at": "2026-07-23T14:05:00Z",
    "source_revision": "git-sha-or-url"
  }
}
```

Key rules:

- `version` is required and monotonically incremented for breaking schema changes.
- `completed_stages` lists stages that finished successfully, in pipeline order.
- `stage_timestamps` maps each completed stage to an ISO 8601 UTC timestamp of
  completion.
- `config_hash` is a SHA-256 of the **canonical JSON serialization** of the
  **unredacted** `config` object. The stored `config` value is a redacted,
  human-readable copy and is **not** used for identity checks. Canonicalization
  rules:
  - Object keys are sorted lexicographically by UTF-8 byte value.
  - Output is UTF-8 encoded.
  - No insignificant whitespace, no trailing newline.
  - Numbers are rendered as JSON numbers (no leading zeros, no trailing decimal
    points, no exponent normalization beyond what the JSON encoder produces).
  - Omitted fields are **not** included in the serialized object and are
    therefore distinct from explicitly present default-valued fields. Callers
    who want hash stability across omitted defaults must normalize the config
    to include defaults before hashing.
- `manifest` is a pointer object (not a plain string) with `path` (relative to
  the checkpoint directory), `sha256` (hash of the compressed manifest file
  bytes), and `size` (compressed file size in bytes).
- The JSON checkpoint stores **file paths and SHA-256 hashes** in the manifest
  pointer and the in-memory index, not full bodies.

### File bundle (`files.ndjson.gz`)

File bodies are stored as a file of **concatenated, independently-decompressible
gzip members**. Each gzip member contains one JSON object on a single line. The
decompressed concatenation is valid newline-delimited JSON.

```json
{"path": "src/main.rs", "sha256": "...", "encoding": "base64", "content": "Zm4gbWFpbig..."}
{"path": "Cargo.toml", "sha256": "...", "encoding": "base64", "content": "..."}
```

Rules:

- `sha256` is the hash of the **exact raw bytes** read from the file, after any
  redaction but before any encoding. `path` is metadata and is **not** included
  in the hash input.
- The raw bytes are preserved without Unicode normalization, transcoding, or
  stripping binary data.
- `encoding` declares how the (possibly redacted) raw bytes are represented in
  JSON. The default and recommended encoding is `base64`.
- `content` is the base64-encoded representation of the bytes described by
  `sha256`. The `sha256` is computed over the decoded bytes, not the base64
  string.
- Each JSON record is compressed as its own gzip member. A reader can seek to a
  member's compressed byte offset and decompress only that member, without
  reading or decompressing any other member.

### In-memory index

During resume, `brigid` scans `files.ndjson.gz` once and builds:

```text
path -> (compressed_byte_offset, compressed_length, sha256)
```

The index stores only offsets, lengths, and hashes. **File bodies are not
loaded** into this index. When a stage needs a body, the reader seeks to
`compressed_byte_offset`, reads `compressed_length` bytes, decompresses the
single gzip member, verifies the per-record `sha256`, decodes `content`, and
passes the bytes to the stage.

### Resume behavior

1. Load `checkpoint.json` and validate `version`.
2. Compute the current `config_hash` from the current run `config` (full,
   unredacted) using the canonical serialization defined above. Compute the
   current source identity (e.g., `git rev-parse HEAD` for local trees or the
   resolved URL/revision for GitHub URLs).
3. If the current `config_hash` differs from `checkpoint.config_hash`, or if
   the current source identity differs from
   `checkpoint.metadata.source_revision`, **discard any cached file bodies** and
   re-crawl. Do not read the old manifest under a mismatched identity.
4. If both identities match and the manifest pointer is valid (`path` exists,
   `sha256` and `size` of the compressed file match), scan `files.ndjson.gz`
   once to build the offset index. **Do not load file bodies** into the index.
5. When a stage needs a file body, use the offset index to seek to the gzip
   member, decompress it, verify the per-record `sha256`, decode `content` from
   its declared `encoding`, and pass the bytes to the stage.
6. Re-crawl only if the manifest is corrupt, missing, or does not match the
   manifest pointer checksum and size.

### Secret and credential handling

Checkpoints must not store secrets or credentials in recoverable form.

- **File bundle redaction**: before writing a file body to `files.ndjson.gz`,
  the crawl stage must skip or redact files matching secret patterns. Path
  patterns include files such as `.env`, `.env.*`, `*secret*`, and
  `*credential*`. Content patterns include common secret-shaped strings such as
  API keys, tokens, and passwords. The `sha256` stored in the bundle is the
  hash of the bytes as they appear in the bundle after any redaction.
- **`config` redaction**: the `config` object written to `checkpoint.json` must
  have any secret values replaced with a placeholder (e.g., `"****"`) or
  omitted entirely. The `config_hash` is always computed over the **full,
  unredacted** config so that identical runtime configurations produce
  identical hashes, but the persisted `config` is only for human inspection.
- The redaction rules are applied consistently on every crawl so that identical
  inputs produce identical hashes and resume remains deterministic.

### Atomic publication and recovery

A checkpoint is published as two files; both must be updated atomically so an
interruption never leaves a readable but inconsistent state.

1. Write the new `files.ndjson.gz` to a temporary path (e.g.
   `files.ndjson.gz.tmp`), `fsync` the file and its containing directory, then
   `rename` it over the old manifest.
2. Compute the manifest `sha256` and compressed `size` over the final file.
3. Write the new `checkpoint.json` to a temporary path (e.g.
   `checkpoint.json.tmp`), `fsync` the file and its containing directory, then
   `rename` it over the old checkpoint.

Recovery rules:

- On startup, validate the manifest pointer in `checkpoint.json` against the
  actual `files.ndjson.gz` (`sha256` and `size` must match).
- If the checkpoint or manifest is truncated, or if the manifest pointer does
  not match the manifest file, treat the checkpoint as corrupt and re-crawl.
- Tools that copy or archive checkpoints must keep `checkpoint.json` and
  `files.ndjson.gz` together and verify the manifest pointer on restore.

## Alternatives Considered

### Monolithic JSON checkpoint

- **Pros**: Single file, trivial to read and write.
- **Cons**: Linear memory and storage growth with total serialized contents;
  oversized files; whole-blob parsing; rewrite and streaming costs. The Python
  reference already hits this wall.
- **Rejected**: Does not meet the bounded-memory and partial-resume goals in the
  product spec.

### Pure content-addressed blob store (one file per blob)

- **Pros**: Deduplication is automatic; easy to verify integrity.
- **Cons**: Large number of small files; filesystem overhead; harder to ship
  and archive as a unit.
- **Rejected**: More complex than needed for v1. `ndjson.gz` keeps all bodies in
  one compressed stream that is easy to version and archive, while the offset
  index and per-record gzip members keep memory bounded.

### Re-crawl on every resume

- **Pros**: No separate manifest to manage.
- **Cons**: Re-reads the entire repository, which is slow and may race with
  working-tree changes.
- **Rejected**: Resume should be deterministic and fast; the manifest is the
  source of truth for the snapshot that produced the checkpoint.

## Consequences

- **Positive**: Checkpoint JSON stays small and fast to parse regardless of
  repository size.
- **Positive**: The manifest is append-friendly, so crawl can append gzip
  members as it discovers files without holding the whole tree in memory.
- **Positive**: Memory usage during resume stays proportional to the number of
  files (the offset index), not the total size of file bodies.
- **Positive**: SHA-256 hashes and source revision checks let the pipeline
  detect changed configurations or sources and re-crawl safely.
- **Positive**: Per-record gzip members and an offset index allow bounded,
  single-member decompression when a body is needed.
- **Negative**: Two files to manage instead of one; tooling must copy/move both
  together and validate the manifest pointer.
- **Negative**: A corrupt `files.ndjson.gz` requires re-crawl, which may be
  expensive; the manifest pointer checksum and atomic write protocol mitigate
  this but do not eliminate it.
- **Negative**: Redaction adds complexity to the crawl stage and must be kept
  consistent across runs to preserve hash stability.

## Related Documents

- `docs/move-to-rust.md` §2.2 and §4.4 — staged pipeline and proposed checkpoint
  shape.
- `docs/best-practices.md` — tutorial quality and security rules that the
  checkpoint stages must preserve.
- `crates/brigid-pipeline/src/lib.rs` — placeholder reference to this ADR.
