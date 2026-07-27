# M1 coverage snapshot (brigid-core + brigid-crawl)

Generated with `cargo llvm-cov --package brigid-core --package brigid-crawl --summary-only`.

| Metric | Value |
|--------|-------|
| Line coverage | **95.06%** |
| Region coverage | 95.17% |
| Function coverage | 97.76% |

M1 exit target was ≥85% line coverage on core + crawl. Hard CI fail remains M2.

Per-file line cover (abbreviated):

- budget.rs 94.53%
- diagrams.rs 95.21%
- eval.rs 93.10%
- mermaid.rs 89.92%
- module.rs 99.32%
- scope.rs 100%
- setup.rs 100%
- crawl local.rs 97.37%
