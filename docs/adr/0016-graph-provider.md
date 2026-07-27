# ADR 0016: Graph Provider Trait for Structural Ground Truth

## Status

Proposed

## Date

2026-07-26

## Context

`brigid` identifies abstractions and relationships via LLM map/reduce. On
small-to-medium codebases this works well — the LLM reads file snippets
and infers module boundaries and dependencies with reasonable accuracy.

On **large codebases** (the original author's use case: new job, huge
inherited codebases), LLM-only identification has three failure modes
that structural tooling can address:

1. **Missed abstractions.** The LLM samples file snippets, not the full
   tree. Abstractions spanning files it never sampled are invisible.
   Community-detection algorithms (Leiden, Louvain) cluster the *entire*
   file graph and surface groupings the LLM would miss.

2. **Invented relationships.** The LLM proposes "A depends on B" from
   reading prose. There is no verification against the actual call graph.
   A claimed relationship might be a hallucination. Symbol-level call
   graphs (from AST parsing) are ground truth the LLM cannot match.

3. **No multimodal input.** `brigid` reads code files only. Real
   codebases have architecture diagrams, design PDFs, whiteboard photos,
   ADR documents. Tools like Graphify extract concepts from these via
   vision models — concepts `brigid` never sees.

### Tools already in the author's environment

The author uses two complementary tools on large codebases:

- **[codegraph](https://github.com/cognition-ai/codegraph)** — a SQLite
  knowledge graph of every symbol, edge, and file in the workspace, built
  via tree-sitter AST parsing. Provides verbatim source, call paths, and
  blast-radius analysis. Sub-millisecond reads. Exposed as an MCP server.
  **Symbol-level structural ground truth.**

- **[Graphify](https://github.com/Graphify-Labs/graphify)** — a Claude
  Code skill that builds a knowledge graph from any input (code, PDFs,
  markdown, screenshots, diagrams, images). Uses tree-sitter + Claude
  vision + Leiden community detection. Produces `graph.json` with
  communities, god nodes (highest-degree concepts), and edges tagged
  `EXTRACTED` / `INFERRED` / `AMBIGUOUS`. **Concept-level clustering +
  multimodal concept extraction.**

### The abstraction-level gap

These tools operate at different abstraction levels than `brigid`:

| Tool | Level | What it produces |
|------|-------|------------------|
| codegraph | Symbol/function | Call graph, blast radius, verbatim source |
| Graphify | Concept (multimodal) | Communities, god nodes, surprising connections |
| brigid | Concept/module | Tutorial chapters with prose and Mermaid diagrams |

Bridging symbol-level data (codegraph) to concept-level output (brigid)
still requires LLM summarization — the graph data *informs* the LLM, it
does not replace the LLM step. The improvement is "better-informed
identification," not "structural certainty." This ADR is honest about
that limit.

### Design constraint: optional, not required

The integration must be **strictly optional**. `brigid` must deliver full
value standalone — no hard dependency on codegraph, Graphify, or any
external graph tool. When a graph provider is present, `brigid` produces
*better* results. When none is present, `brigid` works exactly as today.
This mirrors the plugin pattern (ADR 0014): an extension point that
defaults to no-op.

### Relationship to ADR 0015 (MCP server)

ADR 0015 proposes an MCP server that exposes `brigid`'s checkpoint as
queryable resources/tools. That is a **consumption-side** integration —
the user's AI assistant queries `brigid`'s output.

This ADR is a **production-side** integration — external graph data
improves `brigid`'s generation quality. The two are independent: the
graph provider improves what goes *into* the checkpoint; the MCP server
exposes what comes *out*. Both can ship independently.

## Decision

### 1. `GraphProvider` trait in `brigid-core`

A new module `brigid-core::graph_provider` defines an object-safe trait:

```rust
/// Structural ground truth from an external graph tool.
///
/// Implementations: CodegraphProvider, GraphifyProvider, NoneProvider.
/// When present, the identify and relationships stages use this data
/// to inform and verify LLM output. When absent (NoneProvider), brigid
/// works exactly as today — LLM-only.
pub trait GraphProvider: Send + Sync {
    /// Symbol-level call graph for a file (codegraph).
    /// Returns edges: (caller_symbol, callee_symbol, callee_file).
    fn call_graph_for_file(&self, file_path: &str)
        -> Vec<CallEdge>;

    /// Community-detected file groupings (Graphify Leiden).
    /// Each community is a set of file paths that cluster together.
    fn communities(&self) -> Vec<Community>;

    /// High-degree "god node" concepts (Graphify).
    /// The most-connected concepts in the graph — candidates for
    /// early chapter placement.
    fn hub_concepts(&self) -> Vec<String>;

    /// Verify a claimed relationship exists structurally.
    /// Returns Some(true) if the call graph confirms A→B,
    /// Some(false) if the call graph contradicts it,
    /// None if the provider has no structural data for these nodes.
    fn relationship_exists(&self, from: &str, to: &str)
        -> Option<bool>;

    /// Multimodal concepts extracted from non-code files
    /// (Graphify vision: diagrams, PDFs, images).
    /// Each concept has a source file (e.g., "docs/architecture.png")
    /// and a description.
    fn multimodal_concepts(&self) -> Vec<MultimodalConcept>;

    /// Provider name for diagnostics.
    fn name(&self) -> &str;
}
```

Supporting types (in `brigid-core::graph_provider`):

```rust
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
    pub callee_file: String,
}

pub struct Community {
    pub files: Vec<String>,
    pub label: Option<String>,  // Graphify may provide a label
}

pub struct MultimodalConcept {
    pub concept: String,
    pub source_file: String,     // e.g., "docs/architecture.png"
    pub description: String,
}
```

The trait is **object-safe** (no generics, no `Self` in return position,
`Send + Sync` supertrait) so the pipeline can hold
`Option<Box<dyn GraphProvider>>` and dispatch dynamically across async
stages. This follows the same pattern as `KindDetector` in ADR 0014.

### 2. `NoneProvider` default

`NoneProvider` implements every method as a no-op (empty vectors,
`None` for `relationship_exists`). This is the default when no external
graph tool is configured. The pipeline behavior is identical to today:
LLM-only identification and relationships.

```rust
pub fn none() -> Box<dyn GraphProvider> {
    Box::new(NoneProvider)
}
```

### 3. `CodegraphProvider` adapter

An adapter that queries codegraph's SQLite index (or MCP server) for
symbol-level data:

- `call_graph_for_file` → queries codegraph for all call edges
  originating in or targeting the given file.
- `relationship_exists` → checks whether a structural call path exists
  between two symbols.
- `communities`, `hub_concepts`, `multimodal_concepts` → return empty
  (codegraph is symbol-level, not concept-level).

**Implementation note:** codegraph exposes an MCP server. The adapter
can either (a) query the SQLite `.codegraph/` index directly via `rusqlite`,
or (b) call the MCP server via stdio. Direct SQLite is simpler and has
no process-management overhead; MCP is more decoupled. **Direct SQLite
is recommended for the initial implementation** — the `.codegraph/` index
is a local file, and `rusqlite` is already a common Rust dependency. The
MCP transport can be added later if codegraph's index format becomes
opaque or remote.

### 4. `GraphifyProvider` adapter

An adapter that reads Graphify's `graph.json` output file (not calling
Graphify at runtime — Graphify is a Claude Code skill, not a library):

- `communities` → reads Graphify's Leiden community detection output.
- `hub_concepts` → reads Graphify's "god nodes" (highest-degree
  concepts).
- `multimodal_concepts` → reads concepts extracted from non-code files
  (diagrams, PDFs, images) that `brigid` cannot process itself.
- `call_graph_for_file` → reads Graphify's `EXTRACTED` edges only
  (tree-sitter call graph). `INFERRED` and `AMBIGUOUS` edges are LLM
  guesses — the same problem `brigid` has — and are **not** used as
  structural ground truth.
- `relationship_exists` → checks `EXTRACTED` edges only.

**This adapter reads a file, it does not invoke Graphify.** The user runs
`/graphify .` separately (in Claude Code), which produces `graphify-out/
graph.json`. `brigid` reads that file at generation time. No Claude Code
runtime dependency, no double LLM pass.

### 5. Pipeline integration: identify stage

When a `GraphProvider` is present, the identify stage uses it in two
ways:

**5a. Community-informed abstraction discovery.**

Instead of asking the LLM to discover abstractions from scratch (map/
reduce over file snippets), the identify stage:

1. Gets `provider.communities()` — pre-clustered file groupings.
2. For each community, includes the full file list in the map prompt:
   "These files cluster together structurally: [list]. Name and describe
   the abstraction they represent."
3. The LLM still does the naming and describing (bridging the
   abstraction-level gap), but starts from structural groupings instead
   of guessing from sampled snippets.

Communities are **candidate groupings, not constraints.** The LLM may
merge two communities into one abstraction, split a community, or reject
a community that doesn't represent a meaningful abstraction. The
structural data informs; the LLM decides.

**5b. Multimodal concept injection.**

`provider.multimodal_concepts()` returns concepts extracted from
diagrams, PDFs, and images that `brigid` cannot read. These are injected
into the reduce prompt as additional context: "The following concepts
were extracted from design documents: [list]. Consider whether any
identified abstractions correspond to these documented concepts."

This lets tutorials reference architecture diagrams and design docs the
LLM has never seen — the most genuinely novel contribution of this
integration.

When `NoneProvider` is in use, both 5a and 5b are no-ops and the
identify stage runs exactly as today.

### 6. Pipeline integration: relationships stage

When a `GraphProvider` is present, the relationships stage uses it in
two ways:

**6a. Call-graph-informed relationship proposals.**

The relationships prompt includes structural context from
`provider.call_graph_for_file()` for each abstraction's representative
files: "The call graph shows these structural dependencies: [list].
Propose relationships consistent with this structural data."

The LLM still proposes the relationships (it bridges symbol-level edges
to concept-level descriptions), but it has ground truth to work from
instead of guessing from prose.

**6b. Post-LLM relationship verification.**

After the LLM proposes relationships, each claimed edge is verified via
`provider.relationship_exists()`:

- `Some(true)` → mark the relationship as `verified: structural`.
- `Some(false)` → mark the relationship as `verified: contradicted` and
  emit a warning. The relationship is kept (the LLM may see a conceptual
  dependency the call graph doesn't capture) but flagged.
- `None` → mark as `verified: unknown` (no structural data for these
  nodes).

The verification status is stored in the checkpoint and rendered in
chapters (e.g., a small "✓ verified by call graph" / "? unverified"
marker next to relationship descriptions). This gives the reader
confidence signals about which relationships are structural vs inferred.

When `NoneProvider` is in use, 6a provides no context and 6b marks all
relationships as `verified: unknown` — no behavior change from today.

### 7. Pipeline integration: order stage

When a `GraphProvider` is present, the order stage uses `hub_concepts()`
as a signal: abstractions corresponding to hub concepts are candidates
for earlier placement in the chapter sequence (most-connected concepts
first, so the reader builds mental scaffolding before encountering
leaf abstractions).

The existing ordering logic (LLM proposes, validation checks coverage
and duplicates) is unchanged — hub concepts are an **input signal** to
the LLM ordering prompt, not a constraint.

### 8. Configuration

`RunConfig` gains an optional `graph_provider` field, configured via
`brigid.toml`:

```toml
[graph_provider]
# Optional: enable structural ground truth from external tools.
# When absent, brigid runs LLM-only (NoneProvider default).

# Option A: codegraph (SQLite index)
type = "codegraph"
index_path = ".codegraph/graph.db"

# Option B: Graphify (graph.json output file)
# type = "graphify"
# graph_path = "graphify-out/graph.json"

# Option C: both (composed provider — merges data from both)
# type = "composed"
# providers = ["codegraph:.codegraph/graph.db", "graphify:graphify-out/graph.json"]
```

Environment variable `BRIGID_GRAPH_PROVIDER` overrides the type for
ad-hoc runs. CLI flag `--graph-provider` overrides both.

When the `[graph_provider]` table is absent, `NoneProvider` is used —
**zero configuration required for standalone use.**

### 9. `ComposedProvider` for multi-tool setups

When both codegraph and Graphify are present (the author's environment),
a `ComposedProvider` merges data from both:

- `call_graph_for_file` → codegraph (symbol-level ground truth).
- `communities` → Graphify (Leiden clustering).
- `hub_concepts` → Graphify (god nodes).
- `multimodal_concepts` → Graphify (vision-extracted concepts).
- `relationship_exists` → codegraph first (structural); if `None`,
  fall back to Graphify `EXTRACTED` edges.

This lets users combine symbol-level precision (codegraph) with
concept-level clustering and multimodal input (Graphify) in one run.

## Alternatives Considered

### Option A — MCP composition only (no generation-side integration)

Rely on ADR 0015's MCP server + codegraph's MCP server + Graphify's MCP
mode. The user's AI assistant queries all three at consumption time. No
generation-side integration code.

- **Pros:** Zero integration code; immediate; each tool stays
  independent.
- **Cons:** Does NOT improve `brigid`'s *generation* quality. The
  abstractions and relationships in the tutorial are still LLM-only. The
  AI client has to compose the tools at query time, which is unreliable
  for complex questions.
- **Rejected as the primary approach:** Does not address the stated
  problem (LLM-only identification fails on huge codebases). MCP
  composition is still valuable as a *consumption-side* complement
  (ADR 0015) but does not solve the *production-side* quality gap.

### Option B — Graphify as a pre-processing step (runtime dependency)

Run Graphify first, feed its output into `brigid`'s identify stage as
starting material. Requires Graphify at runtime.

- **Pros:** Best abstraction identification (algorithmic clustering +
  LLM naming); multimodal input.
- **Cons:** Graphify is a **Claude Code skill**, not a standalone
  library. Using it at runtime means requiring Claude Code as a
  dependency, or extracting Graphify's core into a library (significant
  coupling). Two LLM passes (Graphify's + brigid's) doubles cost.
- **Rejected:** Too much coupling. The `GraphifyProvider` adapter (§4)
  reads Graphify's *output file* instead, which gives the same data
  without the runtime dependency.

### Option C — REST API integration instead of a trait

Define an HTTP API that external graph tools implement. `brigid` calls
the API at generation time.

- **Pros:** Language-agnostic (any tool can implement the API); no
  crate-level dependency on tool-specific formats.
- **Cons:** Requires running a server alongside `brigid`; network
  overhead; auth concerns; the user must configure and maintain a
  separate service. For a CLI tool that runs in a terminal, a local
  trait + file-based adapter is simpler.
- **Rejected:** Over-engineered for a CLI tool. The trait + file-based
  adapter pattern (read `.codegraph/graph.db` or `graphify-out/
  graph.json`) is simpler and has no server management.

### Option D — Hard dependency on codegraph

Make codegraph a required dependency. `brigid` always uses the call graph.

- **Pros:** Simplest implementation (no `Option`, no `NoneProvider`).
- **Cons:** Violates the core design principle: `brigid` must deliver
  full value standalone. Users without codegraph cannot use `brigid`.
  Adds a hard dependency on an external tool's index format.
- **Rejected:** Breaks standalone usability. The optional provider
  pattern preserves `brigid`'s independence.

## Consequences

- **Positive:** When codegraph is present, `brigid`'s relationships are
  grounded in structural call-graph data — fewer invented relationships,
  verifiable confidence markers in chapters.
- **Positive:** When Graphify is present, `brigid`'s abstractions are
  informed by algorithmic community detection — fewer missed
  abstractions on large codebases, and tutorials can reference
  architecture diagrams and design docs via multimodal concept
  extraction.
- **Positive:** When both are present (`ComposedProvider`), `brigid`
  combines symbol-level precision with concept-level clustering — the
  best of both.
- **Positive:** `NoneProvider` default means `brigid` delivers full value
  standalone. No hard dependency. Zero configuration for users without
  graph tools.
- **Positive:** The trait is a stable extension point — new graph tools
  (Scip-Tools, LSIF-based tools, custom internal analyzers) can be added
  by implementing `GraphProvider` without touching the pipeline.
- **Negative:** The identify and relationships stages gain conditional
  logic (provider present vs absent). Test matrix grows: brigid with
  NoneProvider, with CodegraphProvider, with GraphifyProvider, with
  ComposedProvider.
- **Negative:** The abstraction-level gap is not eliminated. Symbol-level
  data (codegraph) still requires LLM summarization to become
  concept-level output (brigid). The improvement is "better-informed
  guess," not "structural certainty." This is an inherent limit.
- **Negative:** `GraphifyProvider` depends on Graphify's `graph.json`
  format, which is not a stable schema. Format changes in Graphify will
  require adapter updates. Mitigated by reading only the fields we need
  (communities, god nodes, EXTRACTED edges, multimodal concepts) and
  degrading gracefully on missing fields.
- **Negative:** `CodegraphProvider` depends on codegraph's SQLite schema
  (`.codegraph/graph.db`). Same schema-stability concern.

## Validation Plan

Before implementing the full trait, validate the quality gap on real
codebases:

1. Run `brigid generate` on 3-5 large codebases (the author's new job
   codebases) with `NoneProvider` (current behavior).
2. Read the tutorials. Document specific failures: missed abstractions,
   invented relationships, wrong module boundaries.
3. Run codegraph + Graphify on the same codebases.
4. Manually compare `brigid`'s abstractions/relationships against
   codegraph's call graph and Graphify's communities.
5. If the gap is significant (missed abstractions, false relationships),
   implement the trait. If the gap is small, defer.

**This ADR is Proposed, not Accepted, pending this validation.**

## Future Extension Points

1. **MCP-based providers** — A `McpGraphProvider` that queries codegraph
   or Graphify via MCP instead of reading their local files. Useful when
   the tools run as remote services. Builds on ADR 0015's MCP
   infrastructure.

2. **Custom internal analyzers** — Teams with internal code-analysis
   tools (proprietary call graphs, internal dependency trackers) can
   implement `GraphProvider` to feed their data into `brigid` without
   modifying the pipeline.

3. **LSIF / SCIP support** — A `ScipGraphProvider` reading
   `.scip` / LSIF indexes from `scip-tools` or `lsif-rust`. These are
   language-server-based call graphs, an alternative to codegraph's
   tree-sitter approach.

4. **Provider auto-discovery** — `brigid` detects `.codegraph/` or
   `graphify-out/` in the project root and auto-enables the matching
   provider without explicit configuration. Convenience feature; the
   explicit config (§8) stays as the override.

5. **Verification status in MCP server** — ADR 0015's MCP server exposes
   the `verified: structural / contradicted / unknown` status as a
   resource field, so AI assistants can reason about relationship
   confidence at query time.

6. **Streaming provider updates** — When a provider's data changes
   (codegraph re-indexes, Graphify re-runs), `brigid` could invalidate
   affected stages in the checkpoint and re-run them. Builds on ADR
   0013's incremental infrastructure. Future ADR.

## Related Documents

- [codegraph](https://github.com/cognition-ai/codegraph) — symbol-level
  SQLite knowledge graph (tree-sitter AST)
- [Graphify](https://github.com/Graphify-Labs/graphify) — multimodal
  knowledge graph (tree-sitter + Claude vision + Leiden)
- ADR 0001 — Checkpoint schema v1 (verification status stored here)
- ADR 0013 — Git-diff incremental (provider updates could trigger
  incremental re-runs)
- ADR 0014 — Plugin architecture (same optional-trait + default-no-op
  pattern)
- ADR 0015 — MCP server (consumption-side complement to this
  production-side integration)
- [`ARCHITECTURE.md`](../../ARCHITECTURE.md) — design principles (I/O
  isolation, pure core, optional extensions)
