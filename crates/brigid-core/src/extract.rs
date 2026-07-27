//! Robust YAML/JSON block extraction from messy LLM output.
//!
//! LLMs frequently wrap structured output in prose and fenced code blocks
//! (```yaml / ```json / ```). The helpers in this module strip that wrapping
//! and return the cleanest contiguous structured block as a plain string.
//!
//! **No parsing is performed** — the caller is responsible for deserialising
//! the returned string into YAML or JSON. These functions are pure logic with
//! no I/O and no dependencies beyond `std` and `thiserror`.
//!
//! Behaviour mirrors the Python `utils/context_budget.py::extract_yaml_block`
//! helper:
//!
//! 1. If the text contains a fenced block (```yaml / ```json / bare ```),
//!    extract the content between the first opening fence and its closing ````
//!    and tolerate leading/trailing prose.
//! 2. If there are no fences, fall back to a heuristic that looks for
//!    YAML-like `key: value` lines or the first balanced JSON `{`/`[`.
//! 3. An opening fence with no closing fence yields [`ExtractError::UnbalancedFence`].
//! 4. No structured content at all yields [`ExtractError::NoBlockFound`].

use thiserror::Error;

/// Errors returned by [`extract_yaml_block`] / [`extract_json_block`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExtractError {
    /// No structured block (fenced or bare) could be found in the input text.
    #[error("no structured block found in text")]
    NoBlockFound,
    /// An opening code fence was found but never closed.
    #[error("unbalanced code fence in text")]
    UnbalancedFence,
}

/// Extract a YAML block from potentially messy LLM output.
///
/// The search order is:
/// 1. A ```` ```yaml ```` fenced block.
/// 2. A bare ```` ``` ```` fenced block.
/// 3. Bare YAML (lines matching `^\s*\w+\s*:`).
///
/// # Errors
///
/// Returns [`ExtractError::UnbalancedFence`] when an opening fence has no
/// closing fence, and [`ExtractError::NoBlockFound`] when no structured
/// content can be located.
pub fn extract_yaml_block(text: &str) -> Result<String, ExtractError> {
    // 1. Prefer a ```yaml tagged fence.
    if let Some(result) = extract_fenced(text, "yaml") {
        return result;
    }
    // 2. Fall back to a bare ``` fence.
    if let Some(result) = extract_fenced(text, "") {
        return result;
    }
    // 3. Bare YAML heuristic.
    extract_bare_yaml(text)
}

/// Extract a JSON block from potentially messy LLM output.
///
/// The search order is:
/// 1. A ```` ```json ```` fenced block.
/// 2. A bare ```` ``` ```` fenced block.
/// 3. Bare JSON (the first balanced `{`…`}` or `[`…`]`).
///
/// # Errors
///
/// Returns [`ExtractError::UnbalancedFence`] when an opening fence has no
/// closing fence, and [`ExtractError::NoBlockFound`] when no structured
/// content can be located.
pub fn extract_json_block(text: &str) -> Result<String, ExtractError> {
    // 1. Prefer a ```json tagged fence.
    if let Some(result) = extract_fenced(text, "json") {
        return result;
    }
    // 2. Fall back to a bare ``` fence.
    if let Some(result) = extract_fenced(text, "") {
        return result;
    }
    // 3. Bare JSON heuristic.
    extract_bare_json(text)
}

/// Try to extract content from a fenced code block whose opening tag matches
/// `preferred_tag` (pass `""` for a bare ```` ``` ```` fence).
///
/// Returns `None` when no matching opening fence exists, `Some(Ok(..))` on a
/// successful extraction, and `Some(Err(UnbalancedFence))` when an opening
/// fence is never closed.
fn extract_fenced(text: &str, preferred_tag: &str) -> Option<Result<String, ExtractError>> {
    let lines: Vec<&str> = text.lines().collect();

    // Locate the first opening fence whose tag matches `preferred_tag`.
    let open_idx = lines.iter().enumerate().find(|(_, line)| {
        let trimmed = line.trim();
        fence_tag(trimmed).is_some_and(|tag| tag.eq_ignore_ascii_case(preferred_tag))
    });

    let open_idx = match open_idx {
        Some((idx, _)) => idx,
        None => return None,
    };

    // Locate the first closing fence after the opening (any line starting
    // with ````, lenient enough to accept tagged closings from messy output).
    let close_idx = lines[open_idx + 1..]
        .iter()
        .position(|line| line.trim().starts_with("```"))
        .map(|p| open_idx + 1 + p);

    match close_idx {
        Some(close) => {
            let content = lines[open_idx + 1..close].join("\n");
            Some(Ok(dedent_block(&content)))
        }
        None => Some(Err(ExtractError::UnbalancedFence)),
    }
}

/// Trim a fenced block and remove the common leading whitespace from every
/// non-blank line (uniform indentation is common in messy LLM output and is
/// safe for both YAML — relative indentation is preserved — and JSON).
fn dedent_block(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .min()
        .unwrap_or(0);
    if min_indent == 0 {
        return content.trim().to_string();
    }
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                l.chars().skip(min_indent).collect()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Return the language tag of a fence line, or `None` if the line is not a
/// fence opening. A fence opening is a line that starts with ```` ``` ````;
/// the tag is the trimmed remainder (empty for a bare fence).
fn fence_tag(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed.strip_prefix("```").map(|rest| rest.trim())
}

/// Bare YAML heuristic: if any line looks like a YAML `key:` mapping, return
/// the whole text trimmed. Otherwise [`ExtractError::NoBlockFound`].
fn extract_bare_yaml(text: &str) -> Result<String, ExtractError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ExtractError::NoBlockFound);
    }
    let looks_like_yaml = trimmed.lines().any(is_yaml_key_line);
    if looks_like_yaml {
        Ok(trimmed.to_string())
    } else {
        Err(ExtractError::NoBlockFound)
    }
}

/// Whether a line matches the bare-YAML heuristic `^\s*\w+\s*:`.
fn is_yaml_key_line(line: &str) -> bool {
    let rest = line.trim_start();
    // Require at least one word character for the key.
    let key_end = match rest.find(|c: char| !(c.is_alphanumeric() || c == '_')) {
        Some(e) if e > 0 => e,
        _ => return false,
    };
    // Skip optional whitespace between key and colon.
    rest[key_end..].trim_start().starts_with(':')
}

/// Bare JSON heuristic: find the first `{` or `[` and scan forward to its
/// balanced close (skipping string literals). Returns the slice spanning the
/// balanced value, or [`ExtractError::NoBlockFound`].
fn extract_bare_json(text: &str) -> Result<String, ExtractError> {
    let start = text.find(['{', '[']);
    let start = match start {
        Some(s) => s,
        None => return Err(ExtractError::NoBlockFound),
    };

    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;

    for (pos, c) in text.char_indices() {
        if pos < start {
            continue;
        }
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => stack.push(c),
            '}' | ']' => {
                match stack.pop() {
                    Some(open) if matches!(c, '}' if open == '{') => {}
                    Some(open) if matches!(c, ']' if open == '[') => {}
                    _ => return Err(ExtractError::NoBlockFound),
                }
                if stack.is_empty() {
                    return Ok(text[start..=pos].trim().to_string());
                }
            }
            _ => {}
        }
    }

    Err(ExtractError::NoBlockFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_fenced_yaml() {
        let input = "```yaml\nname: foo\ntier: S\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo\ntier: S");
    }

    #[test]
    fn fenced_yaml_with_language_tag() {
        let input = "```yaml\nname: foo\ntier: S\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo\ntier: S");
    }

    #[test]
    fn fenced_yaml_with_bare_fence() {
        let input = "```\nname: foo\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo");
    }

    #[test]
    fn yaml_with_leading_prose() {
        let input = "Here are the abstractions:\n```yaml\nname: foo\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo");
    }

    #[test]
    fn yaml_with_trailing_prose() {
        let input = "```yaml\nname: foo\n```\nThat's all.";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo");
    }

    #[test]
    fn clean_bare_yaml() {
        let input = "name: foo\ntier: S";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo\ntier: S");
    }

    #[test]
    fn unbalanced_yaml_fence() {
        let input = "```yaml\nname: foo";
        assert_eq!(
            extract_yaml_block(input),
            Err(ExtractError::UnbalancedFence)
        );
    }

    #[test]
    fn no_yaml_block_at_all() {
        let input = "Just some prose with no structure";
        assert_eq!(extract_yaml_block(input), Err(ExtractError::NoBlockFound));
    }

    #[test]
    fn empty_text_yaml() {
        assert_eq!(extract_yaml_block(""), Err(ExtractError::NoBlockFound));
    }

    #[test]
    fn yaml_extra_whitespace_inside_fence() {
        let input = "```yaml\n\n  name: foo\n  tier: S\n\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo\ntier: S");
    }

    #[test]
    fn yaml_multiple_fences_extracts_first() {
        let input = "```yaml\nname: foo\n```\n```yaml\nname: bar\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo");
    }

    #[test]
    fn yaml_prefers_yaml_tag_over_bare_fence() {
        let input = "```\nignored\n```\n```yaml\nname: foo\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo");
    }

    #[test]
    fn clean_fenced_json() {
        let input = "```json\n{\"name\": \"foo\"}\n```";
        assert_eq!(extract_json_block(input).unwrap(), "{\"name\": \"foo\"}");
    }

    #[test]
    fn fenced_json_with_bare_fence() {
        let input = "```\n{\"name\": \"foo\"}\n```";
        assert_eq!(extract_json_block(input).unwrap(), "{\"name\": \"foo\"}");
    }

    #[test]
    fn json_with_leading_prose() {
        let input = "Here is the JSON:\n```json\n{\"name\": \"foo\"}\n```";
        assert_eq!(extract_json_block(input).unwrap(), "{\"name\": \"foo\"}");
    }

    #[test]
    fn json_with_trailing_prose() {
        let input = "```json\n{\"name\": \"foo\"}\n```\nThat's all.";
        assert_eq!(extract_json_block(input).unwrap(), "{\"name\": \"foo\"}");
    }

    #[test]
    fn json_with_nested_objects() {
        let input = "```json\n{\"a\": {\"b\": 1}}\n```";
        assert_eq!(extract_json_block(input).unwrap(), "{\"a\": {\"b\": 1}}");
    }

    #[test]
    fn bare_json_object() {
        let input = "{\"name\": \"foo\"}";
        assert_eq!(extract_json_block(input).unwrap(), "{\"name\": \"foo\"}");
    }

    #[test]
    fn bare_json_array() {
        let input = "[1, 2, 3]";
        assert_eq!(extract_json_block(input).unwrap(), "[1, 2, 3]");
    }

    #[test]
    fn bare_json_with_leading_prose() {
        let input = "Here is the data: {\"a\": 1} done";
        assert_eq!(extract_json_block(input).unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn bare_json_nested_array_in_object() {
        let input = "{\"a\": [1, 2]}";
        assert_eq!(extract_json_block(input).unwrap(), "{\"a\": [1, 2]}");
    }

    #[test]
    fn bare_json_with_string_containing_brace() {
        let input = "{\"a\": \"}\"}";
        assert_eq!(extract_json_block(input).unwrap(), "{\"a\": \"}\"}");
    }

    #[test]
    fn bare_json_with_escaped_quotes_in_string() {
        let input = "{\"msg\": \"he said \\\"hello\\\"\"}";
        assert_eq!(
            extract_json_block(input).unwrap(),
            "{\"msg\": \"he said \\\"hello\\\"\"}"
        );
    }

    #[test]
    fn unbalanced_json_fence() {
        let input = "```json\n{\"name\": \"foo\"}";
        assert_eq!(
            extract_json_block(input),
            Err(ExtractError::UnbalancedFence)
        );
    }

    #[test]
    fn no_json_block_at_all() {
        let input = "Just some prose with no structure";
        assert_eq!(extract_json_block(input), Err(ExtractError::NoBlockFound));
    }

    #[test]
    fn empty_text_json() {
        assert_eq!(extract_json_block(""), Err(ExtractError::NoBlockFound));
    }

    #[test]
    fn json_multiple_fences_extracts_first() {
        let input = "```json\n{\"a\": 1}\n```\n```json\n{\"b\": 2}\n```";
        assert_eq!(extract_json_block(input).unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn json_prefers_json_tag_over_bare_fence() {
        let input = "```\nignored\n```\n```json\n{\"a\": 1}\n```";
        assert_eq!(extract_json_block(input).unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn json_extra_whitespace_inside_fence() {
        let input = "```json\n\n  {\"a\": 1}\n\n```";
        assert_eq!(extract_json_block(input).unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn whitespace_only_text_yaml() {
        assert_eq!(
            extract_yaml_block("   \n  \n"),
            Err(ExtractError::NoBlockFound)
        );
    }

    #[test]
    fn whitespace_only_text_json() {
        assert_eq!(
            extract_json_block("   \n  \n"),
            Err(ExtractError::NoBlockFound)
        );
    }

    #[test]
    fn bare_json_unbalanced_returns_no_block() {
        let input = "{\"a\": 1";
        assert_eq!(extract_json_block(input), Err(ExtractError::NoBlockFound));
    }

    #[test]
    fn fence_with_trailing_whitespace_on_tag_line() {
        let input = "```yaml   \nname: foo\n```";
        assert_eq!(extract_yaml_block(input).unwrap(), "name: foo");
    }
}
