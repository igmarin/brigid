# ADR 0013: Git-Diff Incremental File Detection

## Status

Accepted

## Date

2026-07-30

## Context

`decon-crawl` inventories every file under a repository root on every run.
For large repositories this full walk is wasteful when only a handful of
files changed since the last run (e.g. since the last released tag). CI
pipelines and editor integrations frequently want to analyse **only** the
files that changed since a known baseline — a tag, a commit, or a branch
tip.

We need an incremental detection mechanism that:

1. Accepts an arbitrary git ref (tag, commit hash, or branch name).
2. Returns the set of files that changed between that ref and `HEAD`.
3. Integrates with the existing [`crawl_local`] inventory so callers can
   opt into incremental mode without a separate code path.
4. Avoids adding a heavy native dependency (libgit2) to the crawl crate.
5. Handles merge commits correctly for branch-based workflows.

## Decision

### 1. Shell out to `git` via `std::process::Command`

A new module `decon-crawl::git_diff` provides:

```rust
pub fn changed_files_since(repo_root: &Path, ref_name: &str)
    -> Result<Vec<PathBuf>, GitDiffError>
```

It invokes `git diff --name-only --ignore-submodules <ref>...HEAD` using
`std::process::Command`. No libgit2 / `git2` crate dependency is added.

**Rationale:** The `git2` crate pulls in a native libgit2 build, adding
compile time and a C dependency for a feature that a single `git` shell
invocation already provides. The CLI already assumes `git` is available
on `PATH` for repository workflows.

### 2. Triple-dot range for merge-base semantics

The diff uses the **triple-dot** range `<ref>...HEAD` rather than the
double-dot `<ref>..HEAD`:

- `<ref>..HEAD` (double-dot) lists files that differ between the two
  commit tips. When `HEAD` is a merge commit that brought in a long-lived
  branch, this can list files that were changed on *either* side of the
  divergence — including files that were changed on the ref's side but
  are identical at `HEAD`.
- `<ref>...HEAD` (triple-dot) diffs against the **merge-base** of the two
  commits. This surfaces exactly the files changed on the path from the
  common ancestor to `HEAD`, which is what "files changed since the ref"
  means in practice for branch workflows.

For the common non-merge case (linear history) both ranges produce the
same result, so the triple-dot is strictly more correct.

### 3. Validation before diffing

Before running the diff, the module validates:

- **`git` is installed** — `git --version` succeeds.
- **`repo_root` is a git repository** — `git rev-parse --git-dir`
  succeeds. This is authoritative (worktrees lack a `.git` directory).
- **`ref_name` exists** — `git rev-parse --verify <ref>^{commit}`
  succeeds, ensuring the ref resolves to a commit object.

Each failure maps to a distinct `GitDiffError` variant so callers can
distinguish "not a repo" from "bad ref".

### 4. Path post-processing

The raw `git diff --name-only` output is:

- Trimmed and split into lines.
- Filtered to **existing files** (deleted files are excluded, since the
  crawl inventory only contains files on disk).
- Normalised to `/` separators.
- Sorted and de-duplicated.

### 5. `CrawlOptions` integration

`decon-crawl::local` gains a `CrawlOptions` struct:

```rust
pub struct CrawlOptions {
    pub since: Option<String>,
}
```

and a `crawl_local_with_options(root, options)` function. When
`options.since` is `Some(ref)`, the full crawl is performed and then
filtered to the changed-file set, preserving the parallel `files` /
`sizes` invariant. The existing `crawl_local(root)` function is
unchanged (backwards compatible) and delegates to the default (full)
options.

## Alternatives Considered

### Option A — `git2` crate (libgit2 binding)

Use the `git2` Rust crate to compute diffs in-process.

- **Pros:** No shell-out; pure Rust API; no dependency on `git` being on
  `PATH`.
- **Cons:** Adds a native C dependency (libgit2) to the crawl crate,
  increasing compile time and cross-compilation complexity. The CLI
  already requires `git` on `PATH` for repository workflows.
- **Rejected:** The shell-out approach is simpler and avoids the native
  dependency for a single-command use case.

### Option B — Double-dot range `<ref>..HEAD`

Use the double-dot range.

- **Pros:** Slightly more intuitive for linear history.
- **Cons:** Incorrect for merge commits in branch workflows — lists
  files changed on the ref side that are unchanged at `HEAD`.
- **Rejected:** The triple-dot range is correct in both linear and merge
  scenarios.

### Option C — Separate `crawl_incremental` function (no `CrawlOptions`)

Expose only `changed_files_since` and let callers filter the inventory
themselves.

- **Pros:** Minimal API surface in `local`.
- **Cons:** Every caller re-implements the filter + size-parallel logic;
  risk of drift between the file list and sizes.
- **Rejected:** `CrawlOptions` + `crawl_local_with_options` centralises
  the filtering and keeps the `files`/`sizes` parallel invariant.

## Consequences

- **Positive:** Incremental crawls skip unchanged files, reducing work
  for CI and editor integrations on large repos.
- **Positive:** No new native dependency; `git` shell-out is lightweight.
- **Positive:** `crawl_local` remains backwards compatible; existing
  callers are unaffected.
- **Positive:** Merge commits are handled correctly via triple-dot range.
- **Negative:** Requires `git` on `PATH` for incremental mode (full
  crawl still works without git). This is acceptable given the CLI's
  existing git dependency.
- **Negative:** Submodule changes are ignored (`--ignore-submodules`).
  This is intentional to avoid pulling in external repository state; a
  future ADR may revisit submodule handling if needed.

## Related Documents

- [`crates/decon-crawl/src/git_diff.rs`](../../crates/decon-crawl/src/git_diff.rs) —
  the `changed_files_since` implementation.
- [`crates/decon-crawl/src/local.rs`](../../crates/decon-crawl/src/local.rs) —
  `CrawlOptions` and `crawl_local_with_options`.
- ADR 0001 — Checkpoint schema v1 (the pipeline that consumes the crawl
  inventory).
- Issue #224 — Add git_diff module to decon-crawl.
