# LLM-Generated Frozen Fixture

This directory holds a frozen tutorial fixture produced by the `brigid generate`
pipeline against the `tests/fixtures/umbrella` input repo. It is the golden
output used by the nightly LLM smoke job to detect quality regressions.

## Provenance

- **Input repo:** `tests/fixtures/umbrella` (Elixir umbrella monorepo, 3 child
  apps: alpha, beta, gamma)
- **Model:** DeepSeek (placeholder; refresh with a live run)
- **Date:** 2026-01-15 (placeholder; refresh with a live run)
- **Pipeline:** `brigid generate --dir tests/fixtures/umbrella --output-dir
  <out> --max-abstractions 5` with `BRIGID_MAX_LLM_CALLS=20`
- **Eval score:** 100 (structural eval at threshold 70)

## Contents

- `index.md` — full index with all six sections (how to use, module inventory,
  system map, core concepts map, learning path, chapter list)
- `00_architecture_overview.md` — architecture overview for the multi-app
  monorepo
- `01_umbrella_project_layout.md` — chapter on the root umbrella project
- `02_child_application_structure.md` — chapter on child app structure
- `03_shared_configuration.md` — chapter on shared config
- `04_application_composition.md` — chapter on app composition
- `05_environment_and_secrets.md` — chapter on env vars and secrets

Each chapter follows the 10-section structure (Motivation, Core idea, Mental
model, How to use it, Under the hood, Key files, Connections, Pitfalls,
Summary, Evidence), includes mermaid diagrams, evidence footers, and path
citations grounded in the umbrella fixture repo.

## Known limitations

- This fixture is a hand-crafted placeholder standing in for a live LLM run.
  The structure and content are realistic but were not produced by an actual
  DeepSeek call in this environment.
- The umbrella input repo is intentionally tiny (one module per app), so the
  tutorial describes workspace mechanics rather than deep domain logic.
- Path citations reference the real fixture paths under `apps/`, `config/`,
  and the root `mix.exs`.

## Refreshing the fixture

To regenerate with a live LLM:

```sh
export DEEPSEEK_API_KEY=<your-key>
export BRIGID_MAX_LLM_CALLS=20
cargo run -p brigid-cli -- generate \
  --dir tests/fixtures/umbrella \
  --output-dir /tmp/llm-output \
  --max-abstractions 5
cargo run -p brigid-cli -- eval --out /tmp/llm-output --threshold 70
```

If the new output scores at or above the frozen score, copy it into this
directory and commit. The nightly CI job performs the same comparison and
opens an issue on regression.

## Verification

```sh
cargo run -p brigid-cli -- eval --out tests/fixtures/tutorials/llm-generated --threshold 70
```

Expected: `passed=true` with a score of 100.
