# brigid-mcp

MCP (Model Context Protocol) server exposing `brigid`'s checkpoint knowledge
graph for AI assistants.

See [ADR 0015](../../docs/adr/0015-mcp-server.md) for the design rationale.

This crate is read-only: it loads a `brigid generate` checkpoint directory
into memory and serves its structured data (abstractions, relationships,
chapters, setup guide, architecture overview) over MCP. It does **not**
run pipeline stages or make LLM calls.
