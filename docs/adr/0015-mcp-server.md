# ADR 0015: MCP Server for Codebase Knowledge Querying

## Status

Proposed

## Date

2026-07-26

## Context

`brigid` currently produces a **file-based tutorial**: a directory of
Markdown chapters with Mermaid diagrams, an index, a setup guide, and an
architecture overview. This is valuable for human reading and for AI
assistants that can load files directly (Cursor, Claude, Windsurf all
read Markdown from disk).

However, files have a fundamental limitation: they are **flat prose**.
When an AI assistant needs to answer "which chapter explains
`src/auth/session.rs`?" or "what does the auth module depend on?", it
must either:

1. Load all chapters into context (expensive, often exceeds context
   windows on large codebases), or
2. Rely on the user manually finding the right chapter.

The **structured knowledge graph** that `brigid` already produces —
`Abstraction` entities with file mappings, `Relationship` edges, chapter
ordering, setup/overview content — lives inside the checkpoint
(`checkpoint.json` + `files.ndjson.gz`, ADR 0001 + ADR 0006) as typed
data. This graph is queryable: file→abstraction, abstraction→dependencies,
abstraction→chapter, query→relevance-ranked chapters. Plain Markdown
files cannot express these queries efficiently.

The [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) is
an open standard (Anthropic, ~2024) for exposing tools, resources, and
prompts to AI assistants. MCP servers are adopted by Claude Desktop,
Cursor, Windsurf, Continue, and others. An MCP server backed by `brigid`'s
checkpoint would let a user's AI assistant **query the codebase knowledge
graph on demand** — targeted lookups instead of bulk file loading.

### Why now, and why not in v1.0.0

`brigid` v1.0.0 ships the clean file-based tutorial (README refactor in
PR #252). The MCP server is a **separate product surface** with its own
concerns (transport, lifecycle, client compatibility) and should not
block the v1.0.0 release. This ADR records the decision to build it as a
post-v1.0.0 milestone, scoped as **read-only first** (serve the existing
checkpoint, no re-generation).

### What the architecture already provides

`brigid` is well-positioned for an MCP server because of existing design
decisions:

- **Pure core** (ADR design principle §1): `brigid-core` has no I/O —
  the domain logic (abstractions, relationships, budgeting, scope) is
  reusable by any front-end, not just the CLI.
- **Library + thin CLI layering**: an MCP server is just another
  front-end over the same crates. No new analysis logic is needed.
- **Structured checkpoint** (ADR 0001 + ADR 0006): the knowledge graph
  is already serialized as typed JSON with SHA-256-verified stage
  outputs.
- **JSON output schema** (ADR 0012): `StageOutput<T>` envelopes are
  already defined for every pipeline stage — these map directly to MCP
  resource shapes.

### What an MCP server gives that files don't

| MCP capability | What it gives the AI tool that files don't |
|----------------|---------------------------------------------|
| Tool: `find_abstraction_for_file(path)` | O(1) "which chapter explains this file?" instead of loading all chapters |
| Tool: `dependency_graph(abstraction_name)` | Structured relationship query, not re-reading prose |
| Resource: `checkpoint://abstractions` | Full abstraction list as structured JSON |
| Resource: `checkpoint://relationships` | Relationship graph as data — the AI reasons over edges without parsing prose |
| Tool: `relevance_ranked_context(query)` | "Top 3 chapters for 'how does caching work'" — uses `brigid`'s evidence/budgeting logic |
| Prompt: `onboard_to_codebase` | Pre-built onboarding prompt that loads index + setup + top chapters in order |

This is the **RAG-without-embeddings** play: `brigid` already did the
expensive analysis (identify, relationships, ordering). The MCP server
turns that analysis into a queryable knowledge base the user's AI
assistant can hit on demand.

## Decision

### 1. New crate: `brigid-mcp`

A new workspace crate `crates/brigid-mcp` depends on `brigid-core` and
`brigid-pipeline` (for checkpoint loading). It does **not** depend on
`brigid-cli` or `brigid-llm` — the server is read-only and does not make
LLM calls or run pipeline stages.

```
crates/
  brigid-core/       # pure domain (already exists)
  brigid-crawl/      # filesystem (already exists)
  brigid-llm/        # LLM client (already exists)
  brigid-pipeline/   # orchestration (already exists)
  brigid-cli/        # thin CLI binary (already exists)
  brigid-mcp/        # MCP server (new) — depends on core + pipeline
```

Dependency flow stays strictly downward: `brigid-mcp` → `brigid-pipeline`
→ `brigid-core`. No new edges to `brigid-llm` or `brigid-crawl`.

### 2. New CLI command: `brigid serve`

```bash
brigid serve --checkpoint /path/to/checkpoint [--transport stdio|sse] [--port 3000]
```

- `--checkpoint` (required): path to a `brigid generate` checkpoint
  directory. The server loads it once at startup and serves from memory.
- `--transport stdio` (default): stdio transport for local AI clients
  (Claude Desktop, Cursor local MCP). This is the standard local mode.
- `--transport sse --port 3000`: HTTP/SSE transport for remote or
  multi-client scenarios. Phase 2 — not in the initial implementation.
- The server is **long-running** (until killed), unlike the one-shot CLI
  commands. This is a behavior change from "run once, get files" to
  "configure once, always available."

### 3. Read-only first — no re-generation

The initial server serves the **existing checkpoint** only. It does not:

- Watch the filesystem for codebase changes.
- Re-run pipeline stages.
- Trigger `--since` incremental updates.
- Make LLM calls.

If the checkpoint is stale (the codebase has changed since generation),
the server serves stale data. The user re-runs `brigid generate` to
refresh. **Generative behavior (live re-generation, incremental updates)
is a future phase with its own ADR.**

This keeps the initial scope bounded: the server is a query layer over
existing data, not a live analysis engine.

### 4. MCP capabilities exposed

#### Resources (structured data the AI can read)

| URI | Type | Content |
|-----|------|---------|
| `checkpoint://metadata` | JSON | Checkpoint metadata: config, completed stages, git commit, since ref |
| `checkpoint://abstractions` | JSON | Full `IdentifyResult` — all abstractions with file indices, kinds, tiers, apps |
| `checkpoint://relationships` | JSON | Full `RelationshipsResult` — relationship edges with labels and kinds |
| `checkpoint://chapter-order` | JSON | `ChapterOrder` — the ordered list of abstraction indices |
| `checkpoint://files` | JSON | The file inventory (path, language, size) from the crawl |
| `checkpoint://chapter/{index}` | Markdown | The chapter content for abstraction at position `{index}` |
| `checkpoint://setup-guide` | Markdown | The setup guide (if generated) |
| `checkpoint://architecture-overview` | Markdown | The architecture overview (if generated) |
| `checkpoint://index` | Markdown | The combined tutorial index |

Resources use the existing `StageOutput<T>` schema (ADR 0012) where
applicable, so the JSON shapes are stable and documented.

#### Tools (functions the AI can call)

| Tool | Input | Output | Purpose |
|------|-------|--------|---------|
| `find_abstraction_for_file` | `file_path: String` | `Abstraction` or null | Which abstraction owns this file? O(1) lookup via the file→abstraction index |
| `abstraction_dependencies` | `name: String` | `Vec<Relationship>` | What does this abstraction depend on / what depends on it? |
| `files_for_abstraction` | `name: String` | `Vec<String>` | Which source files belong to this abstraction? |
| `relevance_ranked_chapters` | `query: String, limit: usize` | `Vec<ChapterRef>` | Top-N chapters most relevant to a natural-language query, using `brigid`'s evidence-selection logic |
| `chapter_for_file` | `file_path: String` | `ChapterRef` or null | Direct shortcut: file → chapter (composes `find_abstraction_for_file` + chapter lookup) |
| `list_abstractions` | `filter: Option<KindFilter>` | `Vec<AbstractionRef>` | List abstractions, optionally filtered by kind/tier/app |

#### Prompts (pre-built prompt templates)

| Prompt | What it loads |
|--------|---------------|
| `onboard_to_codebase` | Index + setup guide + top 3 chapters by tier — a complete onboarding starter |
| `explain_file` | Takes a file path; loads the owning chapter + the abstraction's dependencies |
| `deep_dive_abstraction` | Takes an abstraction name; loads its chapter + relationship graph + file list |

### 5. Transport: stdio first, SSE later

**Phase 1 (this ADR): stdio only.** stdio is the standard local MCP
transport — the AI client spawns `brigid serve` as a child process and
communicates over stdin/stdout. This is how Claude Desktop, Cursor, and
other local clients work. No port management, no network exposure, no
auth concerns.

**Phase 2 (future ADR): SSE/HTTP.** For remote or multi-client
scenarios (a shared team server, CI integration), add `--transport sse`
with proper auth. This is deferred because it introduces network
security concerns that the read-only stdio server does not have.

### 6. Checkpoint staleness detection

The server reports staleness via the `checkpoint://metadata` resource,
which includes `git_commit` and `since_ref` (ADR 0013). A tool
`is_checkpoint_stale` compares the recorded `git_commit` against the
current `HEAD` of the codebase (if the original `--dir` is accessible
and is a git repo). If they differ, the metadata resource includes a
`stale: true` flag and a human-readable hint to re-run `brigid generate`.

This does **not** auto-refresh — it only informs. The user decides when
to re-generate.

### 7. Client configuration

Users add `brigid serve` to their AI client's MCP config. Example for
Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "brigid": {
      "command": "brigid",
      "args": ["serve", "--checkpoint", "/path/to/my-project/.brigid-checkpoint"]
    }
  }
}
```

For Cursor, the equivalent goes in `.cursor/mcp.json`. The server is
process-per-client (stdio), so there is no shared state concern.

## Alternatives Considered

### Option A — Serve tutorial Markdown files via a static MCP resource

Expose each chapter as an MCP resource, nothing else. No tools, no
graph queries.

- **Pros:** Minimal implementation — wrap the output directory.
- **Cons:** The AI client can already read Markdown files directly. This
  adds a process and a protocol for zero new capability. Does not expose
  the structured graph.
- **Rejected:** No differentiator over "just read the files." The value
  is in the graph queries, not re-serving prose.

### Option B — Embed the MCP server into `brigid-cli` (no new crate)

Add `brigid serve` directly in `brigid-cli` without a separate
`brigid-mcp` crate.

- **Pros:** One fewer crate; simpler workspace.
- **Cons:** `brigid-cli` is intentionally a thin binary (clap args, exit
  codes). Adding a long-running MCP server with its own transport and
  lifecycle bloats the CLI crate and violates the layering rule. The
  MCP server has different dependencies (MCP SDK) that should not be
  forced on every CLI user.
- **Rejected:** Separation of concerns — the CLI is one-shot, the server
  is long-running. Different lifecycles, different dependencies, different
  crates.

### Option C — Full generative server from day one

Build the server with live filesystem watching, incremental
re-generation via `--since`, and on-demand stage re-runs.

- **Pros:** Always-fresh knowledge graph; no manual re-generation.
- **Cons:** Massively larger scope — filesystem watching, concurrent
  regeneration, LLM call management, cache invalidation, multi-client
  consistency. Introduces LLM dependency (`brigid-llm`) into the server,
  breaking the read-only property. The statefulness problem (when to
  re-generate, how to coordinate across clients) is a research problem
  on its own.
- **Rejected:** Premature. Start read-only, validate that the graph
  queries are valuable, then add generative behavior in a future ADR
  with evidence that users need it.

### Option D — HTTP REST API instead of MCP

Expose the same resources/tools as a REST API (`GET /abstractions`,
`POST /find_abstraction_for_file`, …).

- **Pros:** Broader compatibility (any HTTP client, not just MCP-aware
  AI tools); simpler to test with curl.
- **Cons:** Does not integrate with AI assistants' native tool-calling.
  The user would need a custom integration layer. MCP is the standard
  for this exact use case and is already adopted by the major AI coding
  tools.
- **Rejected:** MCP is the right protocol for AI-assistant integration.
  A REST API could be added later as an alternative transport if
  non-AI-tool consumers emerge.

## Consequences

- **Positive:** Users' AI assistants (Cursor, Claude, Windsurf) can
  query the `brigid` knowledge graph on demand — targeted lookups
  instead of bulk-loading tutorial files. This is a genuine
  differentiator over file-only output.
- **Positive:** The pure-core + checkpoint architecture means the MCP
  server is a thin new front-end, not a rewrite. No new analysis logic.
- **Positive:** Read-only scope keeps the initial implementation bounded
  and avoids the statefulness/LLM-dependency problems of a generative
  server.
- **Positive:** `StageOutput<T>` (ADR 0012) maps directly to MCP resource
  shapes — no new schema design needed.
- **Negative:** New crate and new dependency (MCP SDK, likely
  `rmcp` or `mcp-rust-sdk`). Adds to the workspace build and CI matrix.
- **Negative:** Long-running process semantics differ from the one-shot
  CLI. Users must configure their AI client once and manage the process.
  This is a UX change from "run once, get files."
- **Negative:** MCP is a young standard (~1 year old). Client
  compatibility is a moving target. Ongoing maintenance against a
  shifting spec is expected.
- **Negative:** Stale data risk — the server serves the checkpoint as
  of generation time. If the codebase changes, the knowledge graph is
  out of date until the user re-runs `brigid generate`. Mitigated by
  staleness detection (§6), but not eliminated.

## Future Extension Points

1. **Generative server (Phase 2)** — Filesystem watching + incremental
   re-generation via `--since` (ADR 0013). The server detects codebase
   changes and re-runs affected stages. Requires its own ADR and
   introduces `brigid-llm` dependency.

2. **SSE/HTTP transport (Phase 2)** — `--transport sse` for remote or
   multi-client scenarios. Introduces auth and network security
   concerns. Separate ADR.

3. **Multi-checkpoint server** — Serve multiple codebases from one
   server instance (`--checkpoint dir1 --checkpoint dir2`). Resource
   URIs gain a codebase prefix (`checkpoint://my-project/abstractions`).

4. **Write tools** — Tools that let the AI assistant annotate
   abstractions (add notes, mark a chapter as outdated, suggest a
   re-ordering). Writes go back to the checkpoint. Requires a
   write-capable checkpoint store and conflict resolution.

5. **Streaming chapter generation** — A tool that generates a chapter
   on demand for an abstraction not yet in the checkpoint (e.g., a new
   module added after the last `brigid generate`). Bridges read-only and
   generative modes.

6. **Embeddings integration** — Use the abstraction descriptions as
   embeddings for semantic search over the codebase knowledge graph.
   Complements the keyword-based `relevance_ranked_chapters` tool.

## Related Documents

- [Model Context Protocol specification](https://modelcontextprotocol.io/)
- ADR 0001 — Checkpoint schema v1 (the data the server serves)
- ADR 0006 — File-based checkpoint output storage (stage outputs the
  server reads)
- ADR 0012 — JSON output schema (`StageOutput<T>` maps to MCP resources)
- ADR 0013 — Git-diff incremental (staleness detection uses
  `git_commit` / `since_ref`)
- ADR 0014 — Plugin architecture (the same pure-core + thin-front-end
  pattern)
- [`ARCHITECTURE.md`](../../ARCHITECTURE.md) — crate dependency
  hierarchy and design principles (I/O isolation, pure core)
