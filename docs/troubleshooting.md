# Troubleshooting

Common issues, exit codes, and recovery procedures for `brigid`.

---

## Exit codes

`brigid` maps outcomes to stable exit codes so CI and scripts can branch on
them:

| Code | Meaning | What to do |
|------|---------|------------|
| `0` | Success | — |
| `1` | Generic failure | Check stderr for the error message; usually an unexpected pipeline error |
| `2` | Config / path / I/O error | Verify `--dir` exists, `brigid.toml` is valid TOML, checkpoint path is correct |
| `3` | Budget exhausted | The `max_llm_calls` limit was hit; raise it in `brigid.toml` |
| `4` | LLM provider error | Network, timeout, rate-limit, or parse error from the provider; see [LLM provider issues](#llm-provider-issues) |
| `5` | Cancelled (Ctrl+C / SIGTERM) | A partial checkpoint was saved; re-run the same command to resume |

---

## Checkpoint recovery

Every expensive stage is checkpointed. If a run is interrupted (exit 5) or
fails (exit 1/3/4), re-running the same command resumes from the last
completed stage — completed stages are skipped automatically.

To inspect progress without re-running anything:

```bash
brigid resume --checkpoint /path/to/checkpoint-dir --format json
```

The checkpoint lives in the `--checkpoint-dir` (default: a temp dir under the
output directory) and consists of `checkpoint.json` + `files.ndjson.gz`. See
[ADR 0001](adr/0001-checkpoint-schema-v1.md) and
[ADR 0006](adr/0006-file-based-checkpoint-output-storage.md) for the format.
To start fresh, delete the checkpoint directory and re-run.

---

## LLM provider issues

- **`BRIGID_LLM_API_KEY (or DEEPSEEK_API_KEY) not set`** — No API key found.
  See [Usage Guide → API key setup](usage-guide.md#api-key-setup). Without a
  key, `brigid` falls back to a mock client (useful for offline tests, not for
  real generation).
- **`host '…' is not in the allowed hosts list`** — The `base_url` host is not
  approved to receive the `Authorization` header. Add it via
  `BRIGID_LLM_ALLOWED_HOSTS` or the `[[allowed_hosts]]` table in `brigid.toml`.
- **Rate limits / timeouts (exit 4)** — The client retries with backoff, but
  sustained rate limiting will surface as exit 4. Wait and retry, or switch to
  a provider/model with a higher rate limit.
- **`BRIGID_FORCE_MOCK`** — Setting this to a truthy value forces the mock
  client even when a real key is present, for offline reproducibility.
  Falsy values (`0`, `false`, `no`, `off`, empty/whitespace; case-insensitive)
  do **not** enable mock mode. Unset (the default) is also disabled.

---

## Cache problems

LLM responses are cached on disk (keyed by hash(prompt)+model+provider) so
re-runs with an unchanged prompt are free.

- **Stale / wrong responses** — Clear the cache by deleting the cache dir
  (default: platform cache `/brigid/llm-cache`) or set `BRIGID_NO_CACHE=1` to
  bypass it for a single run.
- **Custom cache location** — Set `BRIGID_LLM_CACHE_DIR=/some/path`.
- **Disk full** — The cache enforces a size limit (default 100 MB) and evicts
  oldest entries; if writes fail, check permissions and free space.

---

## Budget exhaustion (exit 3)

The `max_llm_calls` budget caps total LLM calls per run (fail-closed). If a
large monorepo run hits the limit mid-pipeline:

1. Check the checkpoint with `brigid resume` to see which stages completed.
2. Raise the budget in `brigid.toml` (`max_llm_calls = 500`) or via CLI flag.
3. Re-run the same command — completed stages are skipped, so only the
   remaining calls count against the new budget.

---

## Checkpoint corruption

If a checkpoint is unreadable (truncated `checkpoint.json`, missing
`files.ndjson.gz`, or a SHA-256 mismatch on a stage output file):

1. `brigid resume --checkpoint PATH` will report the error and which stage
   is affected.
2. The safest fix is to **delete the checkpoint directory and re-run** from
   scratch. Partial checkpoints with corrupted stage outputs cannot be
   trusted — file-based stage outputs are SHA-256 verified (ADR 0006), and a
   mismatch means the file was tampered with or written incompletely.
3. If you want to preserve completed stages, you can manually delete only the
   offending stage output file from the checkpoint directory; the stage will
   re-run on the next invocation.

---

## `--since` requires git

The `--since <git-ref>` flag (ADR 0013) shells out to `git` to compute the
files changed since a tag, commit, or branch. If `git` is not installed or
not on `PATH`, the crawl fails with a clear error. The full crawl (without
`--since`) works without `git`. Ensure you are running inside a git
repository — `--since` on a non-repo directory is not supported.

## OpenRouter / provider configuration

- **`provider 'openrouter' requires an explicit model`** — set `model` in
  `brigid.toml`, or `BRIGID_MODEL` / `BRIGID_LLM_MODEL` (e.g. `openai/gpt-4o`).
  The same applies to `provider = "openai"`.
- **`host 'openrouter.ai' is not in the allowed hosts list`** — upgrade to a
  build that includes ADR 0017, or add the host via `BRIGID_LLM_ALLOWED_HOSTS`.
- **Wrong cache hits after switching providers** — cache keys include the
  resolved `provider_name`. OpenRouter entries no longer share keys with
  DeepSeek. Old mislabeled entries age out via LRU.

