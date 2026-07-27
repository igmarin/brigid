//! Prompt template rendering via [`minijinja`].
//!
//! The `brigid` pipeline drives the LLM with Jinja2/minijinja prompt templates
//! stored in the repository `prompts/` directory. This module embeds those
//! templates at compile time with [`include_str!`] (so the released binary
//! does not depend on the on-disk layout) and exposes a small, documented
//! rendering API:
//!
//! - [`PromptId`] enumerates the ten known templates and gives access to their
//!   filename ([`PromptId::as_str`]) and embedded source text
//!   ([`PromptId::template_text`]).
//! - [`PromptRenderer`] holds a [`minijinja::Environment`] preloaded with every
//!   template and renders a [`PromptId`] against a `serde_json::Value` context.
//! - [`PromptError`] is the single error type returned by rendering.
//! - [`sanitize_template_input`] neutralizes raw `{{ }}` / `{% %}` Jinja
//!   syntax in free-text variables so that untrusted user input cannot execute
//!   as template code. **Callers MUST sanitize every free-text variable before
//!   placing it in the render context** — see the security note below.
//!
//! # Security note
//!
//! Per `prompts/README.md`, variables such as `language_instruction`,
//! `lang_note`, and `project_name` are free-text and may carry attacker-
//! controlled content. minijinja does **not** auto-escape non-HTML output, so a
//! value containing `{{ 7 * 7 }}` would be evaluated as an expression when the
//! *outer* template is rendered. [`sanitize_template_input`] breaks the
//! `{{`/`}}` and `{%`/`%}` delimiters by inserting a space (`{ {`, `} }`,
//! `{ %`, `% }`), which is invisible to an LLM reader but prevents Jinja
//! interpretation. Redaction of secrets remains the caller's responsibility.
//!
//! # Missing variables
//!
//! [`PromptRenderer::render`] asks minijinja to raise an error when a variable
//! referenced by the template is absent from the context (the default
//! `undefined_behavior` is `Undefined::SemiStrict`-like for lookups). A missing
//! required variable therefore surfaces as [`PromptError::Render`].

use minijinja::{Environment, UndefinedBehavior};
use thiserror::Error;

/// Identifier for one of the ten embedded prompt templates.
///
/// Each variant corresponds to a file under `prompts/`. Use [`as_str`](Self::as_str)
/// to obtain the filename and [`template_text`](Self::template_text) to obtain
/// the embedded source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptId {
    /// `identify_single_shot.md.j2` — small-repo one-shot abstraction list.
    IdentifySingleShot,
    /// `identify_map.md.j2` — per-batch abstraction identification (map phase).
    IdentifyMap,
    /// `identify_reduce.md.j2` — merge/rank candidates (reduce phase).
    IdentifyReduce,
    /// `analyze_relationships.md.j2` — project summary and relationships.
    AnalyzeRelationships,
    /// `order_chapters.md.j2` — pedagogical chapter ordering.
    OrderChapters,
    /// `chapter_outline.md.j2` — mandatory chapter structure fragment.
    ChapterOutline,
    /// `write_chapter.md.j2` — generate a single tutorial chapter.
    WriteChapter,
    /// `review_chapter.md.j2` — optional quality pass over a chapter.
    ReviewChapter,
    /// `write_setup_guide.md.j2` — onboarding/setup chapter.
    WriteSetupGuide,
    /// `write_architecture_overview.md.j2` — chapter-0 architecture overview.
    WriteArchitectureOverview,
}

impl PromptId {
    /// Returns the template filename (relative to the `prompts/` directory).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdentifySingleShot => "identify_single_shot.md.j2",
            Self::IdentifyMap => "identify_map.md.j2",
            Self::IdentifyReduce => "identify_reduce.md.j2",
            Self::AnalyzeRelationships => "analyze_relationships.md.j2",
            Self::OrderChapters => "order_chapters.md.j2",
            Self::ChapterOutline => "chapter_outline.md.j2",
            Self::WriteChapter => "write_chapter.md.j2",
            Self::ReviewChapter => "review_chapter.md.j2",
            Self::WriteSetupGuide => "write_setup_guide.md.j2",
            Self::WriteArchitectureOverview => "write_architecture_overview.md.j2",
        }
    }

    /// Returns the embedded template source text (via [`include_str!`]).
    #[must_use]
    pub const fn template_text(self) -> &'static str {
        match self {
            Self::IdentifySingleShot => {
                include_str!("../prompts/identify_single_shot.md.j2")
            }
            Self::IdentifyMap => include_str!("../prompts/identify_map.md.j2"),
            Self::IdentifyReduce => include_str!("../prompts/identify_reduce.md.j2"),
            Self::AnalyzeRelationships => {
                include_str!("../prompts/analyze_relationships.md.j2")
            }
            Self::OrderChapters => include_str!("../prompts/order_chapters.md.j2"),
            Self::ChapterOutline => include_str!("../prompts/chapter_outline.md.j2"),
            Self::WriteChapter => include_str!("../prompts/write_chapter.md.j2"),
            Self::ReviewChapter => include_str!("../prompts/review_chapter.md.j2"),
            Self::WriteSetupGuide => include_str!("../prompts/write_setup_guide.md.j2"),
            Self::WriteArchitectureOverview => {
                include_str!("../prompts/write_architecture_overview.md.j2")
            }
        }
    }

    /// Returns an iterator over all known prompt ids, in catalog order.
    const fn all() -> [PromptId; 10] {
        [
            Self::IdentifySingleShot,
            Self::IdentifyMap,
            Self::IdentifyReduce,
            Self::AnalyzeRelationships,
            Self::OrderChapters,
            Self::ChapterOutline,
            Self::WriteChapter,
            Self::ReviewChapter,
            Self::WriteSetupGuide,
            Self::WriteArchitectureOverview,
        ]
    }
}

/// Errors returned by prompt rendering.
#[derive(Debug, Error)]
pub enum PromptError {
    /// The template failed to render (e.g. a missing or invalid variable).
    #[error("template render error: {0}")]
    Render(String),
    /// A template failed to load/parse at renderer construction time. This is
    /// a programming error (the embedded template source is malformed) but is
    /// surfaced as a `Result` rather than a panic so that library crates never
    /// abort the process — only `brigid-cli::main` may terminate.
    #[error("template load error for {id}: {message}")]
    Load {
        /// The template filename that failed to load.
        id: &'static str,
        /// The underlying minijinja error message.
        message: String,
    },
    /// The requested template id was not registered in the environment.
    #[error("template not found: {0}")]
    NotFound(String),
}

/// A renderer that preloads all ten prompt templates into a [`minijinja`]
/// [`Environment`] and renders them against a `serde_json::Value` context.
///
/// Templates are embedded at compile time, so the renderer has no runtime
/// dependency on the `prompts/` directory layout.
pub struct PromptRenderer {
    env: Environment<'static>,
}

impl PromptRenderer {
    /// Creates a new renderer with every [`PromptId`] template registered.
    ///
    /// Auto-escaping is disabled because the templates produce Markdown/YAML
    /// prompts (not HTML); see the module-level security note for why callers
    /// must instead use [`sanitize_template_input`] on free-text variables.
    ///
    /// # Errors
    ///
    /// Returns [`PromptError::Load`] if any embedded template fails to parse.
    /// This is a programming error (the committed template source is
    /// malformed) and is returned as a `Result` rather than panicked on, so
    /// that library crates never abort the process.
    pub fn new() -> Result<Self, PromptError> {
        let mut env = Environment::new();
        // Strict undefined behavior so that a missing required variable raises
        // a render error (surfaced as `PromptError::Render`) instead of
        // silently rendering an empty string. This matches the contract
        // documented in `prompts/README.md`: any variable mismatch causes a
        // render error.
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        for id in PromptId::all() {
            // `add_template` parses the source eagerly; a failure here is a
            // programming error (the embedded template is malformed). Propagate
            // it as `PromptError::Load` instead of panicking — library crates
            // must never abort the process.
            env.add_template(id.as_str(), id.template_text())
                .map_err(|e| PromptError::Load {
                    id: id.as_str(),
                    message: e.to_string(),
                })?;
        }
        Ok(Self { env })
    }

    /// Renders the template identified by `id` with the given `context`.
    ///
    /// `context` is a [`serde_json::Value`] (typically an object) whose keys
    /// match the variable names documented in `prompts/README.md`. A missing
    /// required variable yields [`PromptError::Render`].
    ///
    /// # Errors
    ///
    /// Returns [`PromptError::Render`] if minijinja reports a render error
    /// (missing variable, type mismatch, etc.), or [`PromptError::NotFound`]
    /// if the template id is not registered (should not happen for the built-in
    /// ids).
    pub fn render(&self, id: PromptId, context: &serde_json::Value) -> Result<String, PromptError> {
        let template = self
            .env
            .get_template(id.as_str())
            .map_err(|_| PromptError::NotFound(id.as_str().to_owned()))?;
        let value = minijinja::Value::from_serialize(context);
        template
            .render(value)
            .map_err(|e| PromptError::Render(e.to_string()))
    }
}

/// Neutralizes Jinja/minijinja template syntax in a free-text string.
///
/// Replaces the expression delimiter `{{` with `{ {` and `}}` with `} }`, and
/// the statement delimiter `{%` with `{ %` and `%}` with `% }`. The inserted
/// spaces are visually negligible to an LLM but break Jinja parsing, so the
/// text is emitted verbatim instead of being evaluated as template code.
///
/// **Callers MUST apply this to every free-text variable** (e.g.
/// `language_instruction`, `lang_note`, `project_name`) before placing it in
/// the render context. Secret redaction is a separate, caller-owned concern.
///
/// # Example
///
/// ```
/// use brigid_pipeline::prompts::sanitize_template_input;
/// let raw = "name {{ 7 * 7 }} boom";
/// let safe = sanitize_template_input(raw);
/// assert!(!safe.contains("{{"));
/// assert!(!safe.contains("}}"));
/// ```
#[must_use]
pub fn sanitize_template_input(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            if let Some(&next) = chars.peek() {
                if next == '{' {
                    out.push_str("{ {");
                    chars.next();
                    continue;
                }
                if next == '%' {
                    out.push_str("{ %");
                    chars.next();
                    continue;
                }
            }
            out.push(c);
        } else if c == '}' {
            if let Some(&next) = chars.peek() {
                if next == '}' {
                    out.push_str("} }");
                    chars.next();
                    continue;
                }
            }
            out.push(c);
        } else if c == '%' {
            if let Some(&next) = chars.peek() {
                if next == '}' {
                    out.push_str("% }");
                    chars.next();
                    continue;
                }
            }
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn as_str_matches_template_text_filename_round_trip() {
        for id in PromptId::all() {
            assert!(!id.as_str().is_empty());
            assert!(!id.template_text().is_empty());
        }
    }

    #[test]
    fn renderer_new_is_ok_and_renders() {
        let renderer = PromptRenderer::new().expect("renderer should construct");
        let ctx = json!({"project_name": "x", "context": "", "language_instruction": "",
            "max_abstraction_num": 5, "name_lang_hint": "", "desc_lang_hint": "",
            "file_listing": ""});
        let out = renderer
            .render(PromptId::IdentifySingleShot, &ctx)
            .expect("render");
        assert!(!out.is_empty());
    }

    #[test]
    fn renderer_new_is_deterministic() {
        let a = PromptRenderer::new().expect("a");
        let b = PromptRenderer::new().expect("b");
        let ctx = json!({"project_name": "x", "context": "", "language_instruction": "",
            "max_abstraction_num": 5, "name_lang_hint": "", "desc_lang_hint": "",
            "file_listing": ""});
        let ra = a
            .render(PromptId::IdentifySingleShot, &ctx)
            .expect("a render");
        let rb = b
            .render(PromptId::IdentifySingleShot, &ctx)
            .expect("b render");
        assert_eq!(ra, rb);
    }

    #[test]
    fn sanitize_handles_statement_delimiters() {
        let raw = "{% if x %}bad{% endif %}";
        let safe = sanitize_template_input(raw);
        assert!(!safe.contains("{%"));
        assert!(!safe.contains("%}"));
    }

    #[test]
    fn sanitize_preserves_single_braces() {
        let raw = "function foo() { return 1; }";
        assert_eq!(sanitize_template_input(raw), raw);
    }

    #[test]
    fn sanitize_handles_all_four_delimiters_in_one_pass() {
        let raw = "{{ x }} {% if y %} {{ z }} {% endif %}";
        let safe = sanitize_template_input(raw);
        assert!(!safe.contains("{{"));
        assert!(!safe.contains("}}"));
        assert!(!safe.contains("{%"));
        assert!(!safe.contains("%}"));
        // Single braces and percent signs are preserved.
        assert!(safe.contains("{ {"));
        assert!(safe.contains("} }"));
        assert!(safe.contains("{ %"));
        assert!(safe.contains("% }"));
    }

    #[test]
    fn sanitize_no_patterns_returns_input_unchanged() {
        let raw = "plain text with no template syntax at all";
        assert_eq!(sanitize_template_input(raw), raw);
        // empty string round-trips
        assert_eq!(sanitize_template_input(""), "");
    }

    #[test]
    fn sanitize_exact_output_for_mixed_input() {
        let raw = "a{{b}}c{%d%}e";
        assert_eq!(sanitize_template_input(raw), "a{ {b} }c{ %d% }e");
    }
}
