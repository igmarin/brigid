---
layout: default
title: "Best Practices"
nav_order: 3
---

# Best Practices: AI-Generated Codebase Tutorials

General guidance for building **high-quality, trustworthy tutorials** from real repositories—especially large monorepos, multi-app systems, and engine-style architectures. These practices apply whether you use this project, another agent workflow, or a custom pipeline.

Use this document as a checklist when designing or reviewing AI onboarding docs.

---

## 1. Goals of a good codebase tutorial

A successful tutorial helps a newcomer:

1. **Orient** — What is this system? What problem does it solve?
2. **Map** — How is it carved into apps, packages, or engines?
3. **Setup** — How do I run it locally when official docs are thin?
4. **Deepen** — How do the core concepts work, with real paths and diagrams?
5. **Navigate** — Where do I look next in the repo?

If any of these is missing, onboarding quality drops—especially on large systems.

---

## 2. Scope the analysis before you prompt

### 2.1 Prefer intentional scope over “send the whole monorepo”

| Approach | When to use |
|----------|-------------|
| Full repo | Small libraries, single-package apps |
| App / package subset | Umbrellas, multi-service monorepos, engine-style systems |
| Path filters | Exclude builds, deps, generated assets, minified bundles |

**Practice:** Discover modules first (`apps/*`, `packages/*`, `engines/*`, top-level domains), print an inventory, then analyze either the full set or a selected subset.

### 2.2 Include the right sources

- Prefer **source of truth**: application code, public APIs, routers, domain modules, config that defines runtime behavior.
- Include lightweight architecture docs (ADRs, `doc/`, root README) when they help orientation.
- **Exclude** by default: `node_modules`, `_build`, `deps`, `dist`, coverage, lockfile noise, SPA build chunks, minified JS, vendored blobs.

### 2.3 Engine-like / multi-app systems

Many monorepos behave like loosely coupled **engines** (or a Rails engines layout done inconsistently):

- Document **boundaries** (what each app owns) before deep APIs.
- Prefer **cross-app contracts** (HTTP, events, shared domain types) over listing every internal helper.
- Keep shared root scaffolding (`mix.exs` / workspace root, `config/`, Docker, Makefile) when scoping to a few apps.

---

## 3. Context budgeting (LLMs have limits)

### 3.1 Never assume one prompt can hold the repo

For large codebases:

1. **Truncate** oversized individual files (head + tail with an omission marker).
2. **Batch** by module/app so related files stay together.
3. **Map** candidate concepts per batch.
4. **Reduce** candidates into a global top-N without re-sending all source.
5. **Prioritize** entry points (application/router/public API) over migrations, locales, and pure templates.

### 3.2 Path-only stubs are useful

When a module has hundreds of files, keep:

- Full bodies for the most important files  
- **Path-only stubs** for the rest  

The model still sees structure (“this app has these modules”) without blowing the context window.

### 3.3 Log budgets

Always log estimates before expensive calls:

- file count, character count, token estimate  
- number of batches  
- modules included  

Fail **fast** with a clear message when a single batch is still too large—don’t thrash retries on the same oversized prompt.

---

## 4. Abstraction quality

### 4.1 What makes a good “core abstraction”

Prefer concepts that help a newcomer:

- Domain capabilities and bounded contexts  
- App/package entry points and public surfaces  
- Cross-cutting infrastructure only when central (auth, messaging, tenancy)  

Avoid treating every DTO, migration, or generated file as a chapter.

### 4.2 Enrich each abstraction with metadata

After identification, attach:

| Field | Purpose |
|-------|---------|
| `tier` (S / M / L) | Depth and diagram requirements |
| `kind` | domain, infrastructure, adapter, ui, orchestration, data, … |
| `apps` | Which monorepo apps it touches |
| `entry_files` | Best real paths to open first |

Use heuristics (file count, multi-app span, hub/router signals) plus model hints.

### 4.3 Complexity tiers

| Tier | Signals | Tutorial depth |
|------|---------|----------------|
| **S** | Few files, leaf utility | Short chapter; diagrams optional |
| **M** | Several modules, clear API | Full outline; ≥1 diagram (standard) |
| **L** | Many files, multi-app, hubs/orchestrators | Full outline; structure + sequence diagrams |

---

## 5. Relationships between concepts

### 5.1 Capture more than “uses”

Label edges with a coarse **kind** when possible:

- `calls` — runtime invocation  
- `owns` — composition / lifecycle ownership  
- `publishes` / `consumes` — events/messages  
- `configures` — setup/wiring  
- `ui_for` — presentation over a domain concept  
- `related` — fallback  

### 5.2 Evidence under budget

For relationship analysis, don’t dump every cited file:

- Ensure **each abstraction contributes at least one** strong file  
- Prefer diversity across apps (cross-app signal)  
- Cap total snippet size  

### 5.3 Monorepo-aware summaries

When multiple apps exist, the project summary should explain **collaboration and ownership**, not only in-process call graphs.

---

## 6. Chapter structure (fixed contract)

Free-form chapters vary wildly. Use a **mandatory outline** (translate headings if needed, keep order):

1. **Title** — `Chapter N: Name`  
2. **Motivation** — problem + one concrete use case  
3. **Core idea** — short analogy  
4. **Mental model** — diagram when required  
5. **How to use it** — minimal examples; real paths  
6. **Under the hood** — runtime walkthrough; sequence diagram when required  
7. **Key files** — real repo paths + one-line roles  
8. **Connections** — links to other chapters  
9. **Pitfalls** — 2–4 common mistakes  
10. **Summary** — recap + next chapter link  

### 6.1 Continuity without context bloat

Pass **summaries** of previous chapters (headings + bullets), not full prior chapter text. Reduces cost and contradiction drift.

### 6.2 Grounding (anti-hallucination)

- Prefer **real paths** present in provided snippets.  
- Do not invent modules, apps, or APIs not evidenced in context.  
- If code is simplified for teaching, mark it explicitly (`simplified for teaching`).  
- Keep example code blocks short; split longer flows.  

---

## 7. Mermaid and diagrams

### 7.1 Diagram types that pay off

| Type | Best for |
|------|----------|
| `flowchart` | Structure, dependencies, monorepo apps |
| `sequenceDiagram` | Runtime “what happens when…” |
| Lightweight state diagrams | Lifecycles, jobs, workflows |
| Small class diagrams | Domain entities only (keep tiny) |

### 7.2 Policy by complexity

| Tier | Minimal | Standard | Rich |
|------|---------|----------|------|
| S | 0 | 0 | 1 |
| M | 0 | 1 | 2 |
| L | 1 | 2 | 3 |

For tier **L**, prefer **both** a structural flowchart and a sequence diagram.

### 7.3 Deterministic vs LLM-drawn diagrams

| Source | Use for |
|--------|---------|
| **Deterministic** (code-built) | Index concept map, monorepo app map, learning path, fallbacks |
| **LLM** | Narrative sequence / internals—then **validate** |

Deterministic graphs should be the backbone of the index; LLM diagrams enrich chapters.

### 7.4 Always sanitize Mermaid

Common breakages: raw `"`, `#`, `;`, huge labels, unbalanced brackets, non-English punctuation, too many participants.

**Practices:**

- Short labels (≈30–40 chars)  
- Stable node IDs (`A0`, `App0`)  
- `participant X as Label` for sequences  
- Max ~5–6 participants in teaching sequences  
- Validate lightly; drop or replace invalid blocks  
- If the model under-delivers, **append fallback diagrams** rather than shipping empty visuals  

### 7.5 Index-level diagrams (always useful)

For multi-concept tutorials, the index should include:

1. **How to use this tutorial**  
2. **Module / app inventory** (when monorepo)  
3. **System map** (apps as nodes, cross-app edges when known)  
4. **Core concepts map** (abstractions + relationships)  
5. **Learning path** (ordered chapters)  
6. Chapter list (including Setup / Overview when present)  

---

## 8. Setup documentation when official docs are unclear

Onboarding fails when code is rich but **README/setup is thin or outdated**. Treat setup as a first-class tutorial artifact.

### 8.1 Assess existing docs

Score signals such as:

- Presence and length of README / bootstrap docs  
- Install/bootstrap commands (`mix setup`, `bundle install`, `docker compose`, …)  
- Environment variable documentation  
- How to run locally  
- Prerequisites (language/runtime versions)  

Also scan machine-readable config: Docker Compose, Makefiles, mix/npm/gem manifests, `.env.example`, tool version files.

### 8.2 Decide when to generate a Setup chapter

Generate (or force) a **Setup** guide when:

- Onboarding score is low, or  
- Several gaps are detected, or  
- Multi-app monorepo lacks an umbrella/workspace setup narrative, or  
- Many runtime configs exist but docs don’t explain them  

Allow overrides: always generate / never generate.

### 8.3 What a Setup chapter should contain

Suggested structure:

1. Prerequisites  
2. Install dependencies  
3. Environment configuration  
4. Databases / dependent services  
5. Run locally  
6. Verify the install  
7. Common setup pitfalls  
8. **Where this was inferred from** (real file paths)  

**Rules:**

- Prefer commands evidenced in Makefile, compose files, mix aliases, package scripts, README.  
- Mark uncertainty explicitly (“not found in repo—verify with the team”).  
- Include a simple mermaid **setup flow** (prereq → install → configure → run → verify).  

### 8.4 Placement

- File: e.g. `00_setup.md`  
- Linked from the index **before** concept chapters  
- Mention gaps on the index when docs were assessed as weak  

---

## 9. Architecture overview for large systems

When the repo has multiple apps/packages:

- Emit an **Architecture overview** chapter (e.g. `00_architecture_overview.md`).  
- Cover: system type, how the monorepo is carved up, how apps collaborate, suggested reading order.  
- Include at least one system-level mermaid map.  
- Name only apps present in the inventory—no invented services.  

This is often more valuable than an extra deep chapter on a minor utility.

---

## 10. Ordering chapters

Explain **foundations and entry points first**, then supporting concepts, then internals.

- User-facing or edge entry points early  
- Shared domain concepts before specialized adapters  
- Infrastructure that everything depends on before leaf features—or after the domain if the goal is product understanding first (pick one strategy and stay consistent)  

Validate that **every** abstraction appears exactly once in the order.

---

## 11. Quality gates and review

### 11.1 Automatic gates

After generation, check:

- [ ] Required mermaid count for each chapter tier  
- [ ] Mermaid blocks sanitize/validate  
- [ ] Index contains concept map (and system map if multi-app)  
- [ ] Chapters cite real paths when snippets were provided  
- [ ] Chapters include an **evidence footer** (tier, kind, apps, entry files)  
- [ ] Setup/overview present when policies say they should be  
- [ ] Structural eval script score is acceptable (links, mermaid, citations)  

### 11.2 Optional LLM review pass

A second pass can polish chapters, but:

- Keep structure fixed  
- Do not invent new modules  
- Re-enforce diagram minimums  
- Expect higher cost—make it opt-in  

### 11.3 Human review focus (high leverage)

- Are the top abstractions the ones seniors would teach?  
- Do cross-app relationships match reality?  
- Does Setup actually work on a clean machine?  
- Do diagrams clarify or merely decorate?  

---

## 12. Cost, caching, and operations

### 12.1 Prefer a thin CLI wrapper

Use project recipes (e.g. `make tutorial`, `make dry-run`, `make resume`, `make each-app`, `make eval`) so people do not copy long `python main.py ...` lines. Document the same flags in README; keep Make as the default path.

### 12.2 Dry-run before you spend

Always support a **plan-only** mode that:

- Crawls and applies include/exclude + app scope  
- Prints module inventory and setup-doc score  
- Estimates map batches and rough LLM call count  
- **Does not call the LLM**

This is the cheapest way to validate scope on huge monorepos.

### 12.3 Checkpoints and resume

Long runs should persist state after each pipeline stage (fetch → identify → relationships → order → chapters → setup → overview → combine).

- Resume should skip completed stages  
- Support partial regeneration: only certain chapters, setup only, overview only, index only  
- Cap API spend with a max-LLM-calls budget that fails closed  

### 12.4 Per-app fan-out

For engine-style umbrellas, support generating **one tutorial per app** (`apps/*`) in addition to a scoped multi-app run. Smaller tutorials are often more useful than one giant monorepo book.

### 12.5 Progress and cost visibility

Log stage transitions, LLM call counts (and cache hits), diagram fallbacks, and an end-of-run summary. Silent multi-hour jobs destroy trust.

### 12.6 Caching and models

- **Cache** LLM responses during iteration; disable cache when debugging bad outputs  
- Map-reduce means **many** calls on huge repos—scope with app filters when exploring  
- Prefer long-context-friendly default models; keep provider/model configurable via env  
- Optional temperature control for more deterministic structure  

### 12.7 Evaluate generated output

After generation, run a structural eval (no LLM required):

- Index present with mermaid maps  
- Setup / overview present when expected  
- Mermaid blocks mostly valid  
- Chapters cite real paths and include evidence footers  
- Internal markdown links resolve  

Treat score thresholds as a regression gate in CI when possible.

---

## 13. Language and localization

When generating non-English tutorials:

- Translate narrative, headings (as needed), and diagram **labels**.  
- Keep code identifiers and real paths unchanged.  
- Sanitize Mermaid carefully—accents and long phrases often break renderers; shorten labels.  
- **Localize fixed chrome** on the index and footers (section titles like “Chapters” / “Capítulos”, setup links, attribution, evidence labels)—not only the LLM prose.  
- Be consistent: if the user asked for Spanish, index chrome and evidence footers should match.  

---

## 14. End-to-end pipeline (reference pattern)

A robust workflow looks like this:

```text
0. Optional dry-run (crawl + plan, zero LLM calls)
1. Fetch & filter code (include/exclude, size limits); checkpoint
2. Discover modules / apps; optional scope (--apps) or --each-app fan-out
3. Assess setup documentation quality
4. Identify abstractions (map-reduce if large); checkpoint
5. Enrich abstractions (tier, kind, apps, entry files)
6. Analyze relationships (with kinds + budgeted evidence); checkpoint
7. Order chapters for learning; optional --only-chapters filter; checkpoint
8. Write chapters (fixed outline + diagram policy + grounding + evidence footer)
9. Optional chapter review pass
10. Write Setup guide if docs are weak (or forced); checkpoint
11. Write Architecture overview if multi-app; checkpoint
12. Combine index + chapters (localized chrome, deterministic diagrams, sanitize)
13. Resume from checkpoint / partial regenerate as needed
14. Structural eval on output/
```

---

## 15. Quick checklist (copy/paste)

**Before run**

- [ ] Dry-run reviewed (file counts, apps, estimated calls)?  
- [ ] Scoped to the right apps/paths (`--apps` / exclude)?  
- [ ] Build artifacts and deps excluded?  
- [ ] Diagram richness level chosen?  
- [ ] Language set correctly (incl. localized chrome)?  
- [ ] LLM provider/model and call budget set?  

**After run**

- [ ] Index has maps and a clear reading order  
- [ ] Setup chapter exists if docs were unclear  
- [ ] Overview exists for multi-app systems  
- [ ] Each important concept has a coherent chapter + evidence footer  
- [ ] Mermaid renders in the viewer you use  
- [ ] Real file paths appear in “Key files”  
- [ ] No obvious invented APIs  
- [ ] Structural eval score is acceptable  
- [ ] Checkpoint available if you need to resume or regenerate a slice  

---

## 16. Anti-patterns

| Anti-pattern | Prefer |
|--------------|--------|
| One giant prompt with the whole monorepo | Map-reduce + prioritization |
| Chapters that are only API dumps | Use-case driven teaching |
| Diagrams with 15 participants | 5–6 max for teaching sequences |
| Unvalidated Mermaid in the index | Deterministic + sanitize |
| Ignoring setup because “there’s a README” | Score the README; generate when weak |
| Treating every file as an abstraction | Top concepts + entry files |
| Full previous chapters in context | Short summaries only |
| Invented modules for nicer stories | Ground in provided paths |

---

## 17. Adapting these practices outside this repo

These ideas transfer to any stack:

- **RAG + agents:** same scoping, budgeting, and citation rules  
- **Internal tech-writing bots:** same chapter contract and setup assessment  
- **Architecture review assistants:** system map + relationship kinds  
- **Onboarding checklists:** Setup scoring against real config files  

The implementation details (CLI flags, node names) may differ; the **principles** should not.

---

## 18. Related docs in this project

- Repository root [README.md](../README.md) — install, `make` recipes, flags, resume/eval  
- [Move to Rust](./move-to-rust.md) — system/pipeline design and CLI product vision  
- Example tutorials under `docs/*/` — samples of chapter tone and diagrams  

---

*This guide is meant to evolve. When you find a failure mode in generated tutorials (broken diagrams, hallucinated APIs, missing setup), add it under anti-patterns and quality gates.*
