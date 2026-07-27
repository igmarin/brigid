# M2 coverage baseline (workspace)

**Date:** 2026-07-24  
**Command:** `cargo llvm-cov --workspace --summary-only`  
**M2 gate target:** ≥ **85%** line coverage workspace-wide (hard fail in CI).

## Summary

| Metric | Workspace |
|--------|-----------|
| **Lines** | **92.63%** (2307 lines, 170 missed) |
| Regions | 93.76% |
| Functions | 96.34% |

**Recommendation:** Enable the M2 CI hard gate at **85% workspace line coverage** immediately. Current main is **~7.6 points above** the threshold. No fill-gap ticket is required for the gate itself.

## Per-crate / file (lines)

| Path | Line cover | Notes |
|------|------------|--------|
| brigid-core (all modules) | ~90–100% | Mermaid lowest pure module (~90%) |
| brigid-crawl `local.rs` | 97.37% | |
| brigid-pipeline `dry_run.rs` | 90.74% | |
| brigid-llm `lib.rs` | 100% | scaffold only |
| **brigid-cli `main.rs`** | **64.07%** | Weakest file; still leaves workspace >85% |

## Gaps under 85% (file-level)

| File | Line cover | Action |
|------|------------|--------|
| `crates/brigid-cli/src/main.rs` | 64.07% | Optional follow-up: more assert_cmd error paths / formats. **Not required** to enable gate. |

No package other than the CLI binary main is under 85% line coverage.

## Gate design notes

1. **Threshold:** fail CI when workspace **line** coverage &lt; 85%.
2. **Do not** exclude `brigid-cli` by default; keep pressure on CLI tests while the total stays healthy.
3. If a future PR drops the total under 85%, it must add tests (or justify a temporary waiver in the PR).
4. M1 core+crawl snapshot remains in [`m1-coverage-summary.md`](./m1-coverage-summary.md) (~95% on that subset).

## Reproduce

```bash
cargo llvm-cov --workspace --summary-only
```
