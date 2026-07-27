# Prompt Catalog

Versioned prompt templates ported from the Python reference implementation
(`PocketFlow-Tutorial-Codebase-Knowledge`) for the `brigid` pipeline.

## Template syntax

Files use [minijinja](https://github.com/mitsuhiko/minijinja) / Tera / Jinja2
style placeholders: `{{ variable }}`.  Render a template by supplying all
variables listed in the **Inputs** column.

## Prompt inventory

| Prompt | File | Source | Purpose | Inputs | Output |
|--------|------|--------|---------|--------|--------|
| Identify (single-shot) | `identify_single_shot.md.j2` | `nodes.py` `_single_shot_identify` | Small repos: produce the final list of core abstractions in one LLM call. | `project_name`, `context`, `language_instruction`, `max_abstraction_num`, `name_lang_hint`, `desc_lang_hint`, `file_listing` | YAML list of abstractions with `name`, `description`, `file_indices`. |
| Identify (map) | `identify_map.md.j2` | `nodes.py` `_map_batch` | Large monorepos: identify abstractions for one batch of files. | `batch_idx`, `batch_total`, `project_name`, `module_note`, `context`, `language_instruction`, `per_batch`, `name_lang_hint`, `desc_lang_hint`, `file_listing` | YAML list of candidate abstractions. |
| Identify (reduce) | `identify_reduce.md.j2` | `nodes.py` `_reduce_candidates` | Merge and rank per-batch candidates into the final top-N list. | `project_name`, `module_summary`, `language_instruction`, `max_abstraction_num`, `name_lang_hint`, `desc_lang_hint`, `candidates_blob` | YAML list of final abstractions. |
| Analyze relationships | `analyze_relationships.md.j2` | `nodes.py` `AnalyzeRelationships` | Produce project summary and inter-abstraction relationships. | `project_name`, `list_lang_note`, `abstraction_listing`, `context`, `language_instruction`, `monorepo_instruction`, `lang_hint` | YAML with `summary` and `relationships[]`. |
| Order chapters | `order_chapters.md.j2` | `nodes.py` `OrderChapters` | Decide the best pedagogical order for tutorial chapters. | `project_name`, `list_lang_note`, `abstraction_listing`, `context` | YAML ordered list of abstraction indices. |
| Chapter outline | `chapter_outline.md.j2` | `utils/tutorial_quality.py` `chapter_outline_instructions` | Injected into `write_chapter` to enforce fixed section order, diagram quotas, and grounding rules. | `lang`, `tier`, `diagram_level`, `need` | Markdown fragment with mandatory structure and rules. |
| Write chapter | `write_chapter.md.j2` | `nodes.py` `WriteChapters` | Generate a single tutorial chapter for one abstraction. | `language_instruction`, `project_name`, `abstraction_name`, `chapter_num`, `abstraction_description`, `tier`, `kind`, `apps_line`, `entry_list`, `full_chapter_listing`, `prev_link`, `next_link`, `previous_chapters_summary`, `file_context_str`, `chapter_outline`, `need` | Markdown chapter. |
| Review chapter | `review_chapter.md.j2` | `nodes.py` `WriteChapters._review_chapter` | Optional quality pass over a generated chapter. | `language`, `need`, `have`, `chapter_md` | Corrected Markdown chapter. |
| Write setup guide | `write_setup_guide.md.j2` | `utils/tutorial_quality.py` `setup_guide_prompt` | Generate a setup/onboarding chapter when repo docs are weak. | `project_name`, `score`, `gaps`, `context`, `lang` | Markdown setup chapter. |
| Write architecture overview | `write_architecture_overview.md.j2` | `nodes.py` `WriteArchitectureOverview` | Chapter 0 overview for multi-app / engine-like monorepos. | `lang_note`, `project_name`, `summary`, `inventory`, `abstractions`, `relationships` | Markdown architecture overview. |

## Integration notes

- The `brigid-pipeline` crate must supply the exact variable names listed in the
  **Inputs** column. Any mismatch will cause a minijinja/Tera render error.
- At runtime the templates should be embedded with `include_str!` so the binary
  does not depend on the `prompts/` directory layout at execution time. Full
  production embedding in `brigid-pipeline` / `brigid-llm` is left to the
  integration PR.
- Treat all string variables as untrusted user input: pre-validate or escape
  them before rendering. `language_instruction`, `lang_note`, and similar
  free-text variables must never contain raw `{{ }}` Jinja syntax, or they may
  execute as template code when the prompt is rendered.
- All `context` and file-snippet variables must be redacted of secrets before
  rendering, per `docs/best-practices.md`. The prompts themselves contain no
  secret content; redaction is the caller's responsibility.

## Variable schema

Numeric placeholders are rendered with their string representation, so the
pipeline should pass them as integers (or numeric strings) to avoid values like
`"5"` appearing in the prompt.

| Variable | Expected type / values | Used in |
|---|---|---|
| `batch_idx`, `batch_total` | positive integers, `batch_idx <= batch_total` | `identify_map` |
| `chapter_num` | positive integer | `write_chapter` |
| `diagram_level` | one of `minimal`, `standard`, `rich` | `chapter_outline` |
| `language`, `lang` | lowercase language name, e.g. `english`, `spanish` | several prompts |
| `max_abstraction_num` | positive integer | `identify_single_shot`, `identify_reduce` |
| `need` | non-negative integer | `chapter_outline`, `write_chapter`, `review_chapter` |
| `per_batch` | positive integer | `identify_map` |
| `score` | integer `0–100` | `write_setup_guide` |
| `tier` | one of `S`, `M`, `L` | `chapter_outline`, `write_chapter` |

All language hint variables (`language_instruction`, `lang_note`, `lang_hint`,
`list_lang_note`, `name_lang_hint`, `desc_lang_hint`) are optional strings and
are typically empty for English. All other inputs listed in the inventory table
are required.

## Versioning and tests

- Prompt text changes are **breaking** for snapshot/golden tests because they
  shift rendered output and stable hashes. Commit prompt file changes together
  with any updated test snapshots / fixture expectations, and bump the prompt or
  tool version when editing these files.
- Each prompt has a render test in `crates/brigid-pipeline/tests/prompts.rs`
  that renders the template with a synthetic fixture context and asserts the
  output contains the expected sections / YAML markers. When adding a new
  prompt, add a matching fixture test there.
- Keep prompt files free of code logic; all dynamic values are supplied as
  template variables by the pipeline crates.
