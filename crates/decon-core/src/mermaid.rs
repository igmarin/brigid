//! Mermaid sanitize and light validation.
//!
//! Broken Mermaid is a top tutorial failure mode (`docs/best-practices.md` §7.4).
//! This module is pure: no I/O. Callers feed labels and fenced diagram bodies;
//! helpers shorten labels, strip dangerous characters, emit stable node IDs,
//! cap sequence participants, and lightly validate blocks.
//!
//! M1 does **not** include a full Mermaid parser — heuristics + table-driven
//! tests define behavior.
//!
//! When sequence diagrams declare more than [`MAX_SEQUENCE_PARTICIPANTS`]
//! participants, [`sanitize_mermaid`] keeps the first N declarations and
//! drops message lines that reference removed participant ids so dangling
//! arrows are not left behind.

/// Soft max length for diagram labels (best-practices ≈30–40 chars).
pub const MAX_LABEL_CHARS: usize = 40;

/// Max participants kept in teaching sequence diagrams.
pub const MAX_SEQUENCE_PARTICIPANTS: usize = 6;

/// Characters that commonly break Mermaid when left raw in labels.
const FORBIDDEN_IN_LABEL: &[char] = &['"', '#', ';'];

/// Result of a light validation pass over a Mermaid body (no fences).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidateResult {
    /// `true` when no blocking issues were found.
    pub valid: bool,
    /// Human-readable issues (empty when [`Self::valid`] is `true`).
    pub issues: Vec<String>,
}

/// Shorten and scrub a display label for safe Mermaid use.
///
/// - Trims whitespace and collapses internal runs of space to a single space
/// - Removes `"`, `#`, and `;`
/// - Drops characters outside a conservative printable ASCII set (keeps
///   letters, digits, and common punctuation that renderers accept)
/// - Replaces unbalanced `[` / `]` / `(` / `)` with spaces (then re-collapses)
/// - Truncates to [`MAX_LABEL_CHARS`] on a UTF-8 char boundary
///
/// # Examples
///
/// ```
/// use decon_core::mermaid::sanitize_label;
///
/// assert_eq!(sanitize_label(r#"Foo "bar"; #baz"#), "Foo bar baz");
/// assert!(sanitize_label(&"x".repeat(100)).chars().count() <= 40);
/// ```
#[must_use]
pub fn sanitize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len().min(MAX_LABEL_CHARS + 8));
    let mut last_was_space = true; // trim leading

    for ch in label.chars() {
        if FORBIDDEN_IN_LABEL.contains(&ch) {
            continue;
        }
        // Brackets/parens become spaces (not shape syntax in labels).
        if matches!(ch, '[' | ']' | '(' | ')') || ch.is_ascii_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        if is_safe_label_char(ch) {
            out.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            // Drop non-ASCII / unsafe punctuation as a single space boundary.
            out.push(' ');
            last_was_space = true;
        }
    }

    // Trim leading/trailing whitespace in place to avoid an extra allocation.
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    let leading = out.len() - out.trim_start().len();
    if leading > 0 {
        out.drain(0..leading);
    }
    truncate_chars(&out, MAX_LABEL_CHARS)
}

/// Stable node id: `{prefix}{index}` with a sanitized alphabetic prefix.
///
/// Empty or non-alphabetic prefixes fall back to `N`. Prefix is uppercased and
/// stripped to ASCII letters only (e.g. `"app"` → `"APP"`, `"a-1"` → `"A"`).
///
/// # Examples
///
/// ```
/// use decon_core::mermaid::stable_node_id;
///
/// assert_eq!(stable_node_id("A", 0), "A0");
/// assert_eq!(stable_node_id("App", 3), "APP3");
/// ```
#[must_use]
pub fn stable_node_id(prefix: &str, index: usize) -> String {
    let mut p: String = prefix
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if p.is_empty() {
        p.push('N');
    }
    format!("{p}{index}")
}

/// Build a sequence `participant` line with a sanitized label.
///
/// Emits `participant {id} as {label}` when the label differs from `id`,
/// otherwise `participant {id}`.
///
/// # Examples
///
/// ```
/// use decon_core::mermaid::participant_line;
///
/// assert_eq!(
///     participant_line("A0", "Auth Service"),
///     "participant A0 as Auth Service"
/// );
/// ```
#[must_use]
pub fn participant_line(id: &str, label: &str) -> String {
    let safe_id = sanitize_participant_id(id);
    let safe_label = sanitize_label(label);
    if safe_label.is_empty() || safe_label == safe_id {
        format!("participant {safe_id}")
    } else {
        format!("participant {safe_id} as {safe_label}")
    }
}

/// Cap a list of (id, label) pairs for teaching sequences (max
/// [`MAX_SEQUENCE_PARTICIPANTS`]), emitting participant lines.
///
/// Extra participants are dropped (deterministic: first N kept).
#[must_use]
pub fn sequence_participant_lines(participants: &[(&str, &str)]) -> Vec<String> {
    participants
        .iter()
        .take(MAX_SEQUENCE_PARTICIPANTS)
        .map(|(id, label)| participant_line(id, label))
        .collect()
}

/// Light validation of a Mermaid diagram body (without markdown fences).
///
/// Checks:
/// - Non-empty after trim
/// - First token is a known diagram kind (`flowchart`, `graph`,
///   `sequenceDiagram`, `classDiagram`, `stateDiagram`, `stateDiagram-v2`,
///   `erDiagram`, plus common extras)
/// - No raw `"` or `#` characters
/// - Roughly balanced `[]` and `()`
/// - For `sequenceDiagram`, participant count ≤ [`MAX_SEQUENCE_PARTICIPANTS`]
///
/// # Examples
///
/// ```
/// use decon_core::mermaid::validate_mermaid;
///
/// let ok = validate_mermaid("flowchart LR\n  A0[Start] --> A1[End]");
/// assert!(ok.valid);
///
/// let bad = validate_mermaid("flowchart LR\n  A0[\"broken\"]");
/// assert!(!bad.valid);
/// ```
#[must_use]
pub fn validate_mermaid(source: &str) -> ValidateResult {
    let mut issues = Vec::new();
    let trimmed = source.trim();
    if trimmed.is_empty() {
        issues.push("empty mermaid body".to_owned());
        return ValidateResult {
            valid: false,
            issues,
        };
    }

    let first_line = trimmed.lines().next().unwrap_or("").trim();
    let kind = first_line.split_whitespace().next().unwrap_or("");
    if !is_known_diagram_kind(kind) {
        issues.push(format!("unknown or missing diagram kind: {kind:?}"));
    }

    if trimmed.contains('"') {
        issues.push("raw double-quote characters are not allowed".to_owned());
    }
    if trimmed.contains('#') {
        // Mermaid comments use %% ; bare # often breaks node text.
        issues.push("raw '#' characters are not allowed".to_owned());
    }

    if !brackets_balanced(trimmed, '[', ']') {
        issues.push("unbalanced square brackets".to_owned());
    }
    if !brackets_balanced(trimmed, '(', ')') {
        issues.push("unbalanced parentheses".to_owned());
    }

    if kind == "sequenceDiagram" {
        let count = count_sequence_participants(trimmed);
        if count > MAX_SEQUENCE_PARTICIPANTS {
            issues.push(format!(
                "too many sequence participants: {count} > {MAX_SEQUENCE_PARTICIPANTS}"
            ));
        }
    }

    ValidateResult {
        valid: issues.is_empty(),
        issues,
    }
}

/// Sanitize a Mermaid diagram body for safer rendering.
///
/// - Normalizes line endings to `\n`
/// - Scrubs forbidden characters inside node/edge labels heuristically
/// - Caps sequence participants by dropping excess `participant` lines and
///   any message lines that reference removed ids
/// - Leaves structure mostly intact; pair with [`validate_mermaid`] to decide
///   whether to keep, drop, or replace the block
///
/// # Examples
///
/// ```
/// use decon_core::mermaid::{sanitize_mermaid, validate_mermaid};
///
/// let raw = "flowchart LR\n  A0[Hello \"world\"; #x] --> A1[End]";
/// let clean = sanitize_mermaid(raw);
/// assert!(!clean.contains('"'));
/// assert!(validate_mermaid(&clean).valid);
/// ```
#[must_use]
pub fn sanitize_mermaid(source: &str) -> String {
    // Single-pass line-ending normalization: `\r\n` -> `\n`, lone `\r` -> `\n`.
    let mut normalized = String::with_capacity(source.len());
    {
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' {
                if chars.peek() == Some(&'\n') {
                    // `\r\n` -> `\n`: emit `\n` and consume the `\n`.
                    normalized.push('\n');
                    chars.next();
                } else {
                    // lone `\r` -> `\n`
                    normalized.push('\n');
                }
            } else {
                normalized.push(c);
            }
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let mut participant_count = 0usize;
    let mut kept_ids: Vec<String> = Vec::new();
    let first = normalized
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let kind = first.split_whitespace().next().unwrap_or("");
    let is_sequence = kind == "sequenceDiagram";

    for line in normalized.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            lines.push(String::new());
            continue;
        }

        if is_sequence && is_participant_line(trimmed) {
            participant_count += 1;
            if participant_count > MAX_SEQUENCE_PARTICIPANTS {
                continue; // drop excess declarations
            }
            let decl = sanitize_participant_declaration(trimmed);
            if let Some(id) = participant_id_from_declaration(&decl) {
                kept_ids.push(id);
            }
            lines.push(decl);
            continue;
        }

        if is_sequence {
            if let Some(refs) = sequence_line_participant_refs(trimmed) {
                let ok = refs.iter().all(|r| kept_ids.iter().any(|k| k == r));
                if !ok {
                    continue;
                }
            }
        }

        lines.push(sanitize_diagram_line(trimmed));
    }

    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n").trim_end().to_owned()
}

/// Extract fenced `mermaid` blocks from markdown, sanitize each body, and
/// re-embed. Non-mermaid fences are left unchanged. Invalid blocks after
/// sanitize are replaced with a small valid stub flowchart.
///
/// # Examples
///
/// ```
/// use decon_core::mermaid::sanitize_markdown_mermaid_blocks;
///
/// let md = "Intro\n\n```mermaid\nflowchart LR\n  A0[Ok]\n```\n";
/// let out = sanitize_markdown_mermaid_blocks(md);
/// assert!(out.contains("flowchart LR"));
/// ```
#[must_use]
pub fn sanitize_markdown_mermaid_blocks(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    let open = "```mermaid";
    let close = "```";

    loop {
        if let Some(start) = rest.find(open) {
            out.push_str(&rest[..start]);
            let after_open = &rest[start + open.len()..];
            let after_fence_line = match after_open.find('\n') {
                Some(i) => &after_open[i + 1..],
                None => {
                    out.push_str(open);
                    out.push_str(after_open);
                    break;
                }
            };
            if let Some(end) = after_fence_line.find(close) {
                let body = &after_fence_line[..end];
                let sanitized = sanitize_mermaid(body);
                let valid = validate_mermaid(&sanitized).valid;
                out.push_str("```mermaid\n");
                if valid {
                    out.push_str(&sanitized);
                    if !sanitized.ends_with('\n') {
                        out.push('\n');
                    }
                } else {
                    out.push_str("%% invalid mermaid — replaced by sanitizer\n");
                    out.push_str("flowchart LR\n  X0[Invalid diagram dropped]\n");
                }
                out.push_str(close);
                rest = &after_fence_line[end + close.len()..];
            } else {
                out.push_str(open);
                out.push_str(after_open);
                break;
            }
        } else {
            out.push_str(rest);
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn is_safe_label_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '-'
                | '.'
                | ','
                | ':'
                | '/'
                | '+'
                | '*'
                | '='
                | '?'
                | '!'
                | '@'
                | '&'
                | '%'
                | '\''
        )
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

fn sanitize_participant_id(id: &str) -> String {
    let mut s: String = id.to_string();
    s.retain(|c| c.is_ascii_alphanumeric() || c == '_');
    if s.is_empty() {
        s.push('P');
    }
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s.insert(0, 'P');
    }
    s
}

fn is_known_diagram_kind(kind: &str) -> bool {
    matches!(
        kind,
        "flowchart"
            | "graph"
            | "sequenceDiagram"
            | "classDiagram"
            | "stateDiagram"
            | "stateDiagram-v2"
            | "erDiagram"
            | "gantt"
            | "pie"
            | "journey"
            | "gitGraph"
            | "mindmap"
    )
}

fn brackets_balanced(s: &str, open: char, close: char) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth < 0 {
                return false;
            }
        }
    }
    depth == 0
}

fn count_sequence_participants(source: &str) -> usize {
    source
        .lines()
        .filter(|l| is_participant_line(l.trim()))
        .count()
}

fn is_participant_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("participant ") || t.starts_with("actor ")
}

fn sanitize_participant_declaration(line: &str) -> String {
    let t = line.trim();
    let (kw, rest) = if let Some(r) = t.strip_prefix("participant ") {
        ("participant", r)
    } else if let Some(r) = t.strip_prefix("actor ") {
        ("actor", r)
    } else {
        return sanitize_diagram_line(t);
    };
    let rest = rest.trim();
    if let Some((id, label)) = rest.split_once(" as ") {
        return format!(
            "{kw} {} as {}",
            sanitize_participant_id(id.trim()),
            sanitize_label(label)
        );
    }
    format!("{kw} {}", sanitize_participant_id(rest))
}

fn participant_id_from_declaration(line: &str) -> Option<String> {
    let t = line.trim();
    let rest = t
        .strip_prefix("participant ")
        .or_else(|| t.strip_prefix("actor "))?;
    let id = rest
        .split_once(" as ")
        .map(|(id, _)| id)
        .unwrap_or(rest)
        .trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_owned())
    }
}

/// Best-effort extraction of participant ids from a sequence message/note line.
///
/// Returns `None` for lines that are not message-like (e.g. title, autonumber).
fn sequence_line_participant_refs(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if t.is_empty() || t.starts_with("sequenceDiagram") {
        return None;
    }
    if is_participant_line(t) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("note ") {
        let after = t[5..].trim_start();
        let after = after
            .strip_prefix("left of ")
            .or_else(|| after.strip_prefix("right of "))
            .or_else(|| after.strip_prefix("over "))
            .unwrap_or(after);
        let ids_part = after.split(':').next().unwrap_or("").trim();
        let ids: Vec<String> = ids_part
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        return if ids.is_empty() { None } else { Some(ids) };
    }
    for sep in ["<<->>", "<<-->>", "->>", "-->>", "-)", "--)", "-->", "->"] {
        if let Some((left, right)) = t.split_once(sep) {
            let from = left.split_whitespace().last()?.to_owned();
            let to = right
                .split(':')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned();
            if from.is_empty() || to.is_empty() {
                return None;
            }
            return Some(vec![from, to]);
        }
    }
    None
}

fn sanitize_diagram_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '#' | ';' => {}
            '[' => {
                out.push('[');
                let mut inner = String::new();
                let mut depth = 1;
                for c2 in chars.by_ref().take(512) {
                    if c2 == '[' {
                        depth += 1;
                        inner.push(c2);
                    } else if c2 == ']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        inner.push(c2);
                    } else {
                        inner.push(c2);
                    }
                }
                out.push_str(&sanitize_label(&inner));
                out.push(']');
            }
            '(' => {
                out.push('(');
                let mut inner = String::new();
                let mut depth = 1;
                for c2 in chars.by_ref().take(512) {
                    if c2 == '(' {
                        depth += 1;
                        inner.push(c2);
                    } else if c2 == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        inner.push(c2);
                    } else {
                        inner.push(c2);
                    }
                }
                let scrubbed: String = inner
                    .chars()
                    .filter(|ch| !FORBIDDEN_IN_LABEL.contains(ch))
                    .collect();
                out.push_str(&scrubbed);
                out.push(')');
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn sanitize_label_strips_forbidden_and_truncates() {
        assert_eq!(sanitize_label(r#"Foo "bar"; #baz"#), "Foo bar baz");
        let long = "a".repeat(100);
        assert_eq!(sanitize_label(&long).chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn sanitize_label_drops_brackets_and_collapses_space() {
        assert_eq!(sanitize_label("  Hello   [world]  (x)  "), "Hello world x");
    }

    #[test]
    fn sanitize_label_drops_non_ascii_punctuation() {
        let s = sanitize_label("café—naïve");
        assert!(!s.contains('—'));
        assert!(!s.contains('"'));
    }

    #[test]
    fn stable_node_id_formats() {
        assert_eq!(stable_node_id("A", 0), "A0");
        assert_eq!(stable_node_id("App", 3), "APP3");
        assert_eq!(stable_node_id("", 1), "N1");
        assert_eq!(stable_node_id("12", 0), "N0");
    }

    #[test]
    fn participant_line_with_and_without_alias() {
        assert_eq!(
            participant_line("A0", "Auth Service"),
            "participant A0 as Auth Service"
        );
        assert_eq!(participant_line("A0", "A0"), "participant A0");
    }

    #[test]
    fn sequence_participant_lines_caps_at_max() {
        let owned: Vec<(String, String)> = (0..10)
            .map(|i| (format!("P{i}"), format!("Service {i}")))
            .collect();
        let refs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let lines = sequence_participant_lines(&refs);
        assert_eq!(lines.len(), MAX_SEQUENCE_PARTICIPANTS);
    }

    #[test]
    fn validate_accepts_simple_flowchart() {
        let r = validate_mermaid("flowchart LR\n  A0[Start] --> A1[End]");
        assert!(r.valid, "{:?}", r.issues);
    }

    #[test]
    fn validate_rejects_quotes_and_unknown_kind() {
        let r = validate_mermaid("flowchart LR\n  A0[\"x\"]");
        assert!(!r.valid);
        assert!(r.issues.iter().any(|i| i.contains("quote")));

        let r2 = validate_mermaid("notADiagram\n  A --> B");
        assert!(!r2.valid);
    }

    #[test]
    fn validate_rejects_too_many_sequence_participants() {
        let mut body = String::from("sequenceDiagram\n");
        for i in 0..8 {
            body.push_str(&format!("  participant P{i}\n"));
        }
        body.push_str("  P0->>P1: hi\n");
        let r = validate_mermaid(&body);
        assert!(!r.valid);
        assert!(r.issues.iter().any(|i| i.contains("participants")));
    }

    #[test]
    fn sanitize_mermaid_removes_quotes_and_hash() {
        let raw = "flowchart LR\n  A0[Hello \"world\"; #x] --> A1[End]";
        let clean = sanitize_mermaid(raw);
        assert!(!clean.contains('"'), "{clean}");
        assert!(!clean.contains('#'), "{clean}");
        assert!(!clean.contains(';'), "{clean}");
        let v = validate_mermaid(&clean);
        assert!(v.valid, "{:?}", v.issues);
    }

    #[test]
    fn sanitize_mermaid_drops_extra_participants() {
        let mut raw = String::from("sequenceDiagram\n");
        for i in 0..8 {
            raw.push_str(&format!("  participant P{i} as Service {i}\n"));
        }
        raw.push_str("  P0->>P1: hi\n");
        raw.push_str("  P7->>P0: dangling should go\n");
        let clean = sanitize_mermaid(&raw);
        let count = clean
            .lines()
            .filter(|l| l.trim_start().starts_with("participant "))
            .count();
        assert_eq!(count, MAX_SEQUENCE_PARTICIPANTS);
        assert!(
            !clean.contains("P7"),
            "dangling participant refs must be dropped: {clean}"
        );
        assert!(clean.contains("P0->>P1"), "{clean}");
        assert!(validate_mermaid(&clean).valid, "{clean}");
    }

    #[test]
    fn sanitize_mermaid_empty_input() {
        assert_eq!(sanitize_mermaid(""), "");
        assert_eq!(sanitize_mermaid("   \n  "), "");
    }

    #[test]
    fn sanitize_markdown_fenced_blocks_table() {
        let cases = [
            ("no fences", "Just text", "Just text"),
            (
                "valid flowchart",
                "```mermaid\nflowchart LR\n  A0[Start]\n```",
                "```mermaid\nflowchart LR\n  A0[Start]\n```",
            ),
            (
                "quotes scrubbed",
                "```mermaid\nflowchart LR\n  A0[Say \"hi\"]\n```",
                "```mermaid\nflowchart LR\n  A0[Say hi]\n```",
            ),
        ];
        for (name, input, expected) in cases {
            let out = sanitize_markdown_mermaid_blocks(input);
            assert_eq!(out, expected, "case {name}");
        }
    }

    #[test]
    fn sanitize_markdown_replaces_irrecoverable_block() {
        let md = "```mermaid\nnotAKind\n  A --> B\n```";
        let out = sanitize_markdown_mermaid_blocks(md);
        assert!(out.contains("Invalid diagram dropped"), "{out}");
        assert!(out.contains("flowchart LR"), "{out}");
    }

    #[test]
    fn sequence_notes_and_actors_are_handled() {
        let raw = concat!(
            "sequenceDiagram\n",
            "  actor A0 as User\n",
            "  participant B0 as API\n",
            "  A0->>B0: call\n",
            "  note over A0,B0: handshake\n",
            "  note left of A0: client\n",
        );
        let clean = sanitize_mermaid(raw);
        assert!(clean.contains("actor A0"), "{clean}");
        assert!(clean.contains("participant B0"), "{clean}");
        assert!(clean.contains("note over A0,B0"), "{clean}");
        assert!(validate_mermaid(&clean).valid, "{clean}");

        let mut big = String::from("sequenceDiagram\n");
        for i in 0..8 {
            big.push_str(&format!("  participant P{i}\n"));
        }
        big.push_str("  note over P0,P7: too many\n");
        big.push_str("  note left of P0: ok\n");
        let clean2 = sanitize_mermaid(&big);
        assert!(!clean2.contains("P7"), "{clean2}");
        assert!(clean2.contains("note left of P0"), "{clean2}");
    }

    #[test]
    fn sanitize_label_only_non_ascii_punctuation() {
        assert_eq!(sanitize_label("——…"), "");
    }

    #[test]
    fn sanitize_participant_id_various_inputs() {
        // empty -> fallback P
        assert_eq!(sanitize_participant_id(""), "P");
        // numeric leading -> prefixed with P
        assert_eq!(sanitize_participant_id("123"), "P123");
        // alphanumeric preserved
        assert_eq!(sanitize_participant_id("A0"), "A0");
        assert_eq!(sanitize_participant_id("Auth_0"), "Auth_0");
        // special chars stripped, underscore kept
        assert_eq!(sanitize_participant_id("A-0.B!"), "A0B");
        assert_eq!(sanitize_participant_id("___"), "___");
        // only special chars -> fallback P
        assert_eq!(sanitize_participant_id("---"), "P");
    }

    #[test]
    fn sanitize_mermaid_normalizes_crlf_line_endings() {
        let raw = "flowchart LR\r\n  A0[Start] --> A1[End]\r\n";
        let clean = sanitize_mermaid(raw);
        assert!(
            !clean.contains('\r'),
            "lone CR must be normalized: {clean:?}"
        );
        assert!(clean.contains("flowchart LR\n"));
        assert!(
            validate_mermaid(&clean).valid,
            "{:?}",
            validate_mermaid(&clean).issues
        );
    }

    #[test]
    fn sanitize_mermaid_normalizes_lone_cr_line_endings() {
        let raw = "flowchart LR\r  A0[Start] --> A1[End]\r";
        let clean = sanitize_mermaid(raw);
        assert!(
            !clean.contains('\r'),
            "lone CR must be normalized: {clean:?}"
        );
        assert!(clean.lines().count() >= 2, "{clean:?}");
    }

    #[test]
    fn sanitize_mermaid_preserves_clean_lf_input() {
        let raw = "flowchart LR\n  A0[Start] --> A1[End]\n";
        let clean = sanitize_mermaid(raw);
        assert_eq!(clean, "flowchart LR\n  A0[Start] --> A1[End]");
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        })]

        #[test]
        fn prop_sanitize_label_no_forbidden_chars(
            label in "[\\x00-\\x7f]{0,128}",
        ) {
            let sanitized = sanitize_label(&label);
            for forbidden in FORBIDDEN_IN_LABEL {
                prop_assert!(
                    !sanitized.contains(*forbidden),
                    "sanitized label {:?} contains forbidden char {:?} (input {:?})",
                    sanitized,
                    *forbidden,
                    label,
                );
            }
        }

        #[test]
        fn prop_sanitize_label_le_max_chars(
            label in "[\\x00-\\x7f]{0,200}",
        ) {
            let sanitized = sanitize_label(&label);
            prop_assert!(
                sanitized.chars().count() <= MAX_LABEL_CHARS,
                "sanitized label len {} exceeds MAX_LABEL_CHARS {} (input {:?})",
                sanitized.chars().count(),
                MAX_LABEL_CHARS,
                label,
            );
        }
    }
}
