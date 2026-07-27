# ADR 0017: OpenRouter as a First-Class LLM Provider

## Status

Proposed

## Date

2026-07-27

## Context

[OpenRouter](https://openrouter.ai) is an OpenAI-compatible API gateway that
routes chat-completion requests to many underlying providers (OpenAI,
Anthropic, Google, local models, etc.) behind a single endpoint,
`https://openrouter.ai/api/v1`. It uses the same `/chat/completions` path and
request/response envelope as the OpenAI API, so a generic
`OpenAiCompatibleClient` can talk to it with few or no changes.

However, OpenRouter is not *just* another OpenAI-compatible host. It has
provider-specific conventions that affect configuration, security, and user
experience:

- Model IDs are namespaced (`openai/gpt-4o`, `anthropic/claude-3.5-sonnet`).
- It optionally accepts a `provider` JSON object and `route` field to control
  routing and fallbacks.
- It recommends `HTTP-Referer` and `X-Title` headers for leaderboard / usage
  attribution.
- It can return standard OpenAI responses but adds its own `model` and `usage`
  fields and a different set of error payloads.

`brigid` already has the infrastructure to add OpenRouter support, but that
infrastructure is currently DeepSeek-first and has several gaps that would block
a clean OpenRouter integration.

## Challenges

### 1. `RunConfig.provider` is parsed but not used

`brigid-core::RunConfig` has `provider: Option<String>` and `model: Option<String>`
(`crates/brigid-core/src/config.rs:87-90`), but `build_real_llm_client` in the
CLI ignores `run_config.provider` and builds the client purely from environment
variables (`crates/brigid-cli/src/main.rs:125-154`). Adding OpenRouter means we
must finally wire the `provider` field into client construction, because
OpenRouter cannot be detected reliably from the `base_url` alone once a user
overrides `BRIGID_LLM_BASE_URL`.

### 2. DeepSeek-centric defaults in `OpenAiClientConfig::from_env`

`OpenAiClientConfig::from_env` defaults to:

```rust
base_url: "https://api.deepseek.com/v1"
model:    "deepseek-chat"
```

(`crates/brigid-llm/src/openai_client.rs:119-121`). These defaults are sensible
for DeepSeek but wrong for OpenRouter. If `provider = "openrouter"`, the
client should default to `https://openrouter.ai/api/v1` and *require* an
explicit model, because OpenRouter has no safe universal default that also
respects the user's routing preferences and budget.

### 3. Provider-name heuristic misclassifies OpenRouter

`OpenAiClientConfig::from_env` derives `provider_name` for cache keys like this:

```rust
let provider_name = if base_url.contains("openai") { "openai" } else { "deepseek" };
```

(`crates/brigid-llm/src/openai_client.rs:122-127`). Pointing the same client at
`https://openrouter.ai/api/v1` would classify it as `deepseek`, polluting the
cache key and any provider-specific request shaping. A stable `provider_name`
must come from `RunConfig.provider` or an explicit env variable, not from a
substring guess.

### 4. Host allowlist does not include `openrouter.ai`

`DEFAULT_ALLOWED_HOSTS` is:

```rust
const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
    "api.openai.com",
    "api.deepseek.com",
    "localhost",
    "127.0.0.1",
];
```

(`crates/brigid-llm/src/openai_client.rs:65-71`). Without adding
`openrouter.ai` here (or requiring the user to add it via
`BRIGID_LLM_ALLOWED_HOSTS`), `validate_host` will reject the OpenRouter base URL
before any request is sent.

### 5. Request body is fixed

The `ChatRequest` body sent today contains only `model`, `messages`, and
`stream` (`crates/brigid-llm/src/openai_client.rs:289-303`). OpenRouter works
with that, but to use routing controls (`provider`, `route`, `transforms`,
`max_price`, etc.) the request body must become extensible. A provider-aware
client needs a way to inject provider-specific JSON fields without turning the
whole request into an untyped blob.

### 6. OpenRouter-specific headers are missing

OpenRouter documentation recommends sending:

- `Authorization: Bearer <OPENROUTER_API_KEY>` (already supported via
  `bearer_auth`).
- `HTTP-Referer` (site URL for rankings).
- `X-Title` (app name for rankings).

`OpenAiCompatibleClient` currently only sends `Authorization` and the JSON
`Content-Type` (`crates/brigid-llm/src/openai_client.rs:360-365`). We should add
the optional `Referer` / `X-Title` headers when the target host is OpenRouter,
otherwise OpenRouter may rate-limit or reject requests that lack attribution.

### 7. API-key fallback is DeepSeek-specific

`OpenAiClientConfig::from_env` looks for `BRIGID_LLM_API_KEY`, then falls back
to `DEEPSEEK_API_KEY` (`crates/brigid-llm/src/openai_client.rs:113-118`). For
OpenRouter users, a natural fallback is `OPENROUTER_API_KEY`, just as DeepSeek
users expect `DEEPSEEK_API_KEY`.

### 8. Two environment variables for allowed hosts

`OpenAiClientConfig::from_env` reads `BRIGID_LLM_ALLOWED_HOSTS`
(`crates/brigid-llm/src/openai_client.rs:132`), while
`brigid_core::config_from_env_map` reads `BRIGID_ALLOWED_HOSTS`
(`crates/brigid-core/src/config.rs:370`). `build_real_llm_client` passes
`run_config.allowed_hosts` into `with_allowed_host`, but a user reading the docs
may set the wrong variable. OpenRouter support should consolidate this into one
obvious variable or clearly document which takes precedence.

### 9. Cache key must stay stable across OpenRouter routing

`CacheKeyInput` includes `provider` and `model`
(`crates/brigid-llm/src/cache.rs:32-44`). With the correct `provider_name =
"openrouter"` and a model like `openai/gpt-4o`, the cache key is stable. If the
provider name is misdetected as `deepseek`, a request for the same prompt and
model could collide with a DeepSeek cached response. Fixing challenge #3 is
therefore a prerequisite.

### 10. Wiremock tests assert a fixed request body

Existing `openai_client` tests match `{"model":"...","messages":[...],"stream":false}`
(`crates/brigid-llm/src/openai_client.rs:558-568`). Adding OpenRouter-specific
body fields (or even unconditional extra fields) will break those assertions
unless the fields are only added when the provider is OpenRouter and the tests
are updated with OpenRouter-specific cases.

### 11. User-facing documentation is DeepSeek/OpenAI only

`docs/usage-guide.md` covers DeepSeek, OpenAI, Ollama, and LM Studio. It does
not explain OpenRouter model namespacing, routing, or the need for explicit
models. The `brigid init` template also only comments `provider` as
`"openai", "deepseek"` (`crates/brigid-cli/src/main.rs:1212`).

### 12. Security and privacy implications

OpenRouter is an intermediary. When `brigid` sends source code to
`openrouter.ai`, OpenRouter may then forward it to a third-party provider chosen
by the model string or the `provider` routing object. The existing host
allowlist only validates the first hop (`openrouter.ai`); it cannot verify the
ultimate provider. Users must be warned that OpenRouter routing means data may
leave OpenRouter's infrastructure, and that the `provider` routing object can
change which jurisdiction or company processes the prompt.

### 13. Error handling is mostly compatible but not complete

OpenRouter returns standard HTTP status codes and an OpenAI-compatible response
shape for successes. Errors include fields like `error.code` and `error.type`
that the current client does not parse; it only stores the raw truncated body
(`crates/brigid-llm/src/openai_client.rs:477-486`). This is acceptable for now,
but we should verify that 402 Payment Required, 429 Rate Limit, and 500-class
errors from OpenRouter are surfaced clearly and do not leak the API key.

### 14. `RunConfig.model` is also ignored

Like `provider`, `RunConfig.model` is parsed and merged but never passed into
`OpenAiClientConfig` construction. For OpenRouter, the user will almost always
need to set `model` explicitly, so this field must also become operational.

## Decision

### Recommended approach: extend `OpenAiCompatibleClient` with provider presets

Add a small `ProviderPreset` concept inside `brigid-llm` (either an enum or a
set of string conventions) that `OpenAiClientConfig` and
`build_real_llm_client` use to pick defaults, headers, and request-body extras.
Do **not** create a separate `OpenRouterClient`; the request/response shape is
still OpenAI-compatible, and duplication is not justified today. If OpenRouter
diverges later, we can revisit a dedicated client.

#### 1. Wire `RunConfig.provider` and `RunConfig.model` into client construction

`build_real_llm_client` should read `run_config.provider` and `run_config.model`
and pass them into `OpenAiClientConfig` construction. If the user sets
`provider = "openrouter"` in `brigid.toml` or `BRIGID_PROVIDER` in the
environment, the client should use OpenRouter defaults; otherwise it should keep
today's DeepSeek-first behavior.

#### 2. Provider-aware defaults

Introduce a provider preset lookup keyed by the normalized provider string:

| provider | default `base_url` | default `model` | `provider_name` | allowed host added |
|---|---|---|---|---|
| `deepseek` | `https://api.deepseek.com/v1` | `deepseek-chat` | `deepseek` | `api.deepseek.com` |
| `openai` | `https://api.openai.com/v1` | none (must set) | `openai` | `api.openai.com` |
| `openrouter` | `https://openrouter.ai/api/v1` | none (must set) | `openrouter` | `openrouter.ai` |
| `custom` / unset | `BRIGID_LLM_BASE_URL` or DeepSeek default | `BRIGID_LLM_MODEL` or DeepSeek default | derived from `base_url` | `BRIGID_LLM_ALLOWED_HOSTS` |

For `openrouter`, the model must be explicit. Refusing to start with a DeepSeek
default is safer than silently sending requests to `deepseek-chat` on
OpenRouter.

#### 3. Add `openrouter.ai` to the default host allowlist

`DEFAULT_ALLOWED_HOSTS` gains `openrouter.ai`. This removes the need for every
OpenRouter user to set an env variable just to allow the host.

#### 4. Add OpenRouter attribution headers

When `provider_name == "openrouter"`, include:

```text
HTTP-Referer: https://github.com/igmarin/brigid
X-Title: brigid
```

Make these overridable via `BRIGID_LLM_REFERER` / `BRIGID_LLM_APP_TITLE` or
drop them if the user disables attribution. The default values point to the
main repository.

#### 5. Optional OpenRouter request-body extras

Add an `extra_body: Option<serde_json::Value>` or strongly typed routing struct
to `OpenAiClientConfig`. For the first implementation, leave it `None` by
default; the model string is enough to get OpenRouter working. A follow-up can
expose `provider.order`, `route`, `max_price`, etc. through `brigid.toml`
without changing the `LlmClient` trait.

#### 6. API-key fallback chain

`OpenAiClientConfig::from_env` should look for keys in this order:

1. `BRIGID_LLM_API_KEY`
2. Provider-specific key (`OPENROUTER_API_KEY` when `provider=openrouter`,
   `DEEPSEEK_API_KEY` when `provider=deepseek`, etc.)
3. Generic `DEEPSEEK_API_KEY` (maintains backward compatibility)

#### 7. Consolidate allowed-host env vars

Keep `BRIGID_LLM_ALLOWED_HOSTS` as the single LLM-specific env variable; treat
`BRIGID_ALLOWED_HOSTS` as an alias or deprecated duplicate. Update
`docs/usage-guide.md`, `docs/troubleshooting.md`, and the man page to use
`BRIGID_LLM_ALLOWED_HOSTS` consistently.

#### 8. Cache key provider name

Use the resolved preset name (`openrouter`, `openai`, `deepseek`) as
`provider_name` for `CacheKeyInput`. This makes cache keys correct and keeps
cache entries from colliding across providers.

#### 9. Validation

When `provider=openrouter`:

- Require `model` to be set (env or config). Error with a clear message if it
  is missing.
- Warn if the model string does not contain a `/`, because OpenRouter model IDs
  are `provider/model`. Do not reject it, because OpenRouter also supports
  unnamespaced model aliases.

#### 10. Testing

Add Wiremock tests for OpenRouter:

- Success with `openai/gpt-4o` model string.
- Host validation passes for `openrouter.ai`.
- `HTTP-Referer` and `X-Title` headers are sent.
- Missing `model` when `provider=openrouter` errors clearly.
- Cache key uses `openrouter` provider name.

Update existing tests only if the change to `ChatRequest` adds unconditional
fields; otherwise keep them unchanged.

### Alternatives Considered

#### A. Purely documentation: "set `BRIGID_LLM_BASE_URL` to OpenRouter"

- **Pros**: No code changes.
- **Cons**: Fails in practice. `provider_name` would be misclassified as
  `deepseek`, `openrouter.ai` would be rejected by the allowlist, the default
  `deepseek-chat` model would be sent to OpenRouter, and the required
  `HTTP-Referer` / `X-Title` headers would be missing. Users would have to
discover these problems one at a time.
- **Rejected**.

#### B. New `OpenRouterClient` struct implementing `LlmClient`

- **Pros**: Clean separation if OpenRouter diverges from the OpenAI
  request/response shape; provider-specific logic is isolated.
- **Cons**: Duplicates retry/backoff/timeout, caching, and host-validation code
  already in `OpenAiCompatibleClient`. The `LlmClient` trait is intentionally
  small (`complete`), so a separate struct adds more boilerplate than value.
- **Rejected** for the first implementation; re-evaluate if OpenRouter adds
  non-OpenAI endpoints or response shapes.

#### C. Make every setting fully manual and generic

Drop all provider presets and require the user to set `base_url`, `model`,
`allowed_hosts`, and optional headers manually.

- **Pros**: Maximum flexibility; no provider-specific code.
- **Cons**: Bad UX for the common case; contradicts the existing `provider`
  field in `RunConfig`; still requires fixing the `provider_name` heuristic and
  allowlist.
- **Rejected**.

## Consequences

### Positive

- OpenRouter becomes a first-class provider with a single `provider =
  "openrouter"` line in `brigid.toml` or `BRIGID_PROVIDER` env variable.
- `RunConfig.provider` and `RunConfig.model` finally drive the live client
  construction, closing a long-standing gap.
- Cache keys remain correct and provider-isolated.
- The same `LlmClient` trait and `bounded_complete` concurrency path are reused
  unchanged.
- Future providers (Groq, Together, local proxies) can be added by adding new
  presets rather than new client structs.

### Negative

- `OpenAiClientConfig` becomes slightly more complex as it carries provider
  presets and optional headers/extras.
- The `provider_name` heuristic in `from_env` must be replaced, which may
  change cache keys for users who currently rely on the substring detection
  (`api.openai.com` → `openai`, everything else → `deepseek`). We should treat
  this as a bug fix and document it.
- OpenRouter routing means the `Authorization` header and prompt leave the
  user's machine and may be forwarded to a third-party provider. This is
  inherent to the service, but we must document it clearly.

### Unresolved

- Should `provider` be typed as an enum (`deepseek`, `openai`, `openrouter`,
  `custom`) or stay a free string? A free string keeps config files forward
  compatible; an enum gives better error messages. Recommendation: keep the
  config format as a string but add a typed internal preset lookup that
  normalizes case and returns a helpful error for unknown values.
- Should we expose OpenRouter's `provider` routing object in `brigid.toml` in
  v1, or defer it to a follow-up? Recommendation: defer; the model string is
  enough for the common case and keeps the ADR scope bounded.

## Related Documents

- `docs/adr/0002-async-trait-llm-client.md` — `LlmClient` trait design.
- `crates/brigid-llm/src/client.rs` — `LlmClient` trait.
- `crates/brigid-llm/src/openai_client.rs` — `OpenAiCompatibleClient` and
  `OpenAiClientConfig`.
- `crates/brigid-llm/src/cache.rs` — `CacheKeyInput` and `cache_key`.
- `crates/brigid-core/src/config.rs` — `RunConfig` and config resolution.
- `crates/brigid-cli/src/main.rs` — `build_real_llm_client` and `init` config
  template.
- `docs/usage-guide.md` — provider setup examples.
