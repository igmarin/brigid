# ADR 0008: Two-Tier Golden Fixture Strategy for Eval Regression

## Status

Accepted

## Date

2026-07-25

## Context

The `brigid eval` subcommand scores tutorial output trees on structural quality
(mermaid validity, link resolution, evidence footers, path citations, index
presence, setup/overview coverage). As the pipeline evolves through M4 and
beyond, we need a regression gate that catches quality degradation — but the
nature of LLM-generated content makes traditional golden testing difficult.

Issue #138 (M4-EVL-1) requires an eval regression gate in CI. Issue #151
(M4-SMK-1) requires a live full-pipeline smoke test. These have conflicting
constraints:

- **CI on every PR** must be fast, deterministic, and free (no API key, no
  network).
- **LLM output verification** requires a real API call, costs money, and is
  non-deterministic (the same prompt may yield different prose).

A single fixture strategy cannot satisfy both.

### Constraints

- PR CI must not require an API key or make network calls.
- The regression gate must catch structural degradation (missing diagrams,
  broken links, absent evidence footers) without depending on exact prose.
- LLM-generated content is non-deterministic, so byte-for-byte comparison is
  impossible. The eval score (a weighted structural checklist) is the stable
  signal.
- The nightly LLM smoke must detect quality regression over time — if a
  pipeline change causes the live output to score significantly lower than the
  frozen baseline, a human should investigate.

## Decision

Adopt a **two-tier golden fixture strategy**:

### Tier 1: Hand-crafted fast CI fixtures (every PR)

Two hand-crafted tutorial trees live in `tests/fixtures/tutorials/`:

- **`good-mini`** — a well-formed tutorial with valid Mermaid, resolving
  links, evidence footers, path citations, setup guide, and architecture
  overview. Scores at or above the eval threshold (80).
- **`broken-mini`** — a deliberately degraded tutorial missing diagrams,
  with broken links, no evidence footers. Scores below the threshold.

The CI `eval-regression` job runs:

```bash
# Positive: good-mini must pass at threshold 80
cargo run -p brigid-cli -- eval --out tests/fixtures/tutorials/good-mini --threshold 80

# Negative: broken-mini must fail below threshold 80
cargo run -p brigid-cli -- eval --out tests/fixtures/tutorials/broken-mini --threshold 80
# (non-zero exit expected; CI asserts this)
```

This gate is deterministic, requires no API key, and runs in seconds. It
catches regressions in the **eval scoring logic itself** (e.g. a bug that
makes `eval` accept broken tutorials) and validates that the structural
checklist still discriminates good from bad output.

### Tier 2: LLM-generated frozen fixture + nightly CI verification

A frozen LLM-generated tutorial tree lives in
`tests/fixtures/tutorials/llm-generated/`. It was produced by running
`brigid generate` against the `umbrella` fixture with a live DeepSeek key and
committing the output. Its eval score is the **baseline** for nightly
comparison.

The CI `nightly-llm` job (scheduled at 4 AM UTC, never on PR/push):

1. Restores the LLM disk cache from a previous run (to reduce API cost on
   unchanged prompts).
2. Runs `brigid generate --dir tests/fixtures/umbrella --output-dir /tmp/llm-output`
   with a live `DEEPSEEK_API_KEY`.
3. Evals the live output and the frozen fixture.
4. Compares scores: if the live score is **below** the frozen score, opens a
   GitHub issue labelled `nightly-regression` and fails the job.
5. If the live score is at or above the frozen score, the job passes.

This catches **pipeline quality regression** — if a code change causes the
same input to produce structurally worse output (e.g. missing diagrams,
shorter chapters, broken links), the nightly job detects it within 24 hours.

### Why two tiers

| Concern | Tier 1 (hand-crafted) | Tier 2 (LLM-generated) |
|---------|----------------------|----------------------|
| Runs on | Every PR | Nightly schedule only |
| API key required | No | Yes (DeepSeek) |
| Cost | Free | Cents per run (cached) |
| Deterministic | Yes (fixed files) | No (LLM output varies) |
| Catches | Eval scoring regressions; structural discrimination | Pipeline quality regressions over time |
| Fixtures | `good-mini`, `broken-mini` | `llm-generated` (frozen baseline) |

Tier 1 guards the **eval tool itself**. Tier 2 guards the **pipeline output
quality**. Neither alone is sufficient: Tier 1 cannot detect that a pipeline
change produces worse chapters (it tests fixed files, not live output), and
Tier 2 cannot run on every PR (it needs an API key and is non-deterministic).

## Alternatives Considered

### Single tier: only hand-crafted fixtures

- **Pros**: Fast, free, deterministic, runs on every PR.
- **Cons**: Cannot detect pipeline quality regression. If a prompt change or
  stage refactor causes chapters to lose diagrams or evidence footers, the
  hand-crafted fixtures still pass — they are static files, not live output.
- **Rejected**: Leaves the most important regression class (pipeline output
  quality) unguarded.

### Single tier: only LLM-generated nightly

- **Pros**: Catches pipeline quality regression.
- **Cons**: No fast feedback on PR. A developer could break the eval scoring
  logic itself and not find out until the nightly run (up to 24 hours later).
  Also, the eval logic regression would make the nightly comparison
  meaningless (both scores would be wrong).
- **Rejected**: Too slow for the eval-logic guard; circular if eval itself is
  broken.

### Snapshot testing of LLM output (byte-for-byte)

- **Pros**: Maximum precision.
- **Cons**: LLM output is non-deterministic. The same prompt + model can
  produce different valid tutorials. Byte-for-byte comparison would produce
  constant false positives.
- **Rejected**: Fundamentally incompatible with LLM non-determinism.

### Score-only nightly without a frozen fixture

- **Pros**: Simpler — just assert live score >= threshold.
- **Cons**: A slow quality drift (score drops from 90 to 75 over several
  changes, each small) would not be caught if the threshold is 70. Comparing
  against the frozen fixture's score detects relative regression, not just
  absolute failure.
- **Rejected**: Less sensitive to gradual degradation.

## Consequences

- **Positive**: PR CI gets fast, deterministic eval regression feedback
  without API keys.
- **Positive**: Nightly CI catches pipeline quality regression within 24 hours,
  with automatic issue creation for investigation.
- **Positive**: The frozen `llm-generated` fixture serves as documentation of
  what a good LLM-generated tutorial looks like at a point in time.
- **Negative**: The nightly job costs a few cents per run (mitigated by LLM
  disk cache). If the cache is cold (prompt changed), the cost is higher.
- **Negative**: The frozen fixture must be manually refreshed when the
  pipeline intentionally changes output quality (e.g. a prompt improvement
  that changes chapter structure). This is a deliberate human action, not
  automated.
- **Negative**: Non-deterministic LLM output means the nightly job may
  occasionally fail due to a bad random generation, not a real regression.
  Re-running the job usually resolves this. The issue-creation flow surfaces
  these for human triage.

## Related Documents

- `tests/fixtures/tutorials/good-mini/` — Tier 1 positive fixture.
- `tests/fixtures/tutorials/broken-mini/` — Tier 1 negative fixture.
- `tests/fixtures/tutorials/llm-generated/` — Tier 2 frozen baseline.
- `.github/workflows/ci.yml` — `eval-regression` job (Tier 1) and `nightly-llm`
  job (Tier 2).
- `crates/brigid-core/src/eval.rs` — the eval scoring logic both tiers exercise.
- Issue #138 (M4-EVL-1) — Tier 1 eval regression gate.
- Issue #151 (M4-SMK-1) — Tier 2 nightly LLM smoke.
