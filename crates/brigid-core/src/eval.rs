//! Structural tutorial evaluation (no LLM).
//!
//! Scores a tutorial tree of markdown files for the M1 quality gate
//! (`docs/best-practices.md` §12.7): index + mermaid maps, setup/overview,
//! mermaid validity, path citations / evidence footers, and internal links.
//!
//! Pure: callers inject relative paths + file contents. Filesystem loading
//! lives in the CLI / pipeline wrappers.

use crate::mermaid::{sanitize_mermaid, validate_mermaid};
use std::collections::BTreeSet;

/// Default pass threshold for structural eval (0–100).
pub const DEFAULT_EVAL_PASS_THRESHOLD: i32 = 70;

/// Weights for each structural check (sum = 100).
pub const WEIGHT_INDEX_PRESENT: i32 = 20;
/// Index contains at least one mermaid fence.
pub const WEIGHT_INDEX_MERMAID: i32 = 15;
/// Setup or overview chapter present.
pub const WEIGHT_SETUP_OR_OVERVIEW: i32 = 15;
/// Mermaid validity ratio across all blocks.
pub const WEIGHT_MERMAID_VALID: i32 = 20;
/// Chapters cite real-looking repo paths.
pub const WEIGHT_PATH_CITATIONS: i32 = 15;
/// Evidence / grounding footer signals.
pub const WEIGHT_EVIDENCE_FOOTER: i32 = 10;
/// Internal markdown links resolve within the tree.
pub const WEIGHT_LINKS_RESOLVE: i32 = 5;

/// One markdown file in a tutorial output tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TutorialFile {
    /// Path relative to the tutorial root (POSIX `/`, e.g. `index.md`).
    pub path: String,
    /// Full file contents.
    pub content: String,
}

/// Structural evaluation report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvalReport {
    /// Aggregate score 0–100.
    pub score: i32,
    /// Whether [`Self::score`] meets the pass threshold.
    pub passed: bool,
    /// Threshold used for [`Self::passed`].
    pub threshold: i32,
    /// Per-check results.
    pub checks: EvalChecks,
    /// Human-readable failure reasons (empty when all checks full credit).
    pub reasons: Vec<String>,
}

/// Individual check outcomes used for scoring and debugging.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvalChecks {
    /// `index.md` (or `INDEX.md`) present.
    pub has_index: bool,
    /// Index body contains a mermaid fence.
    pub index_has_mermaid: bool,
    /// Setup or overview chapter detected by filename or H1.
    pub has_setup_or_overview: bool,
    /// Total mermaid fences found in the tree.
    pub mermaid_block_count: usize,
    /// Mermaid fences that validate after sanitize.
    pub mermaid_valid_count: usize,
    /// At least one chapter (non-index) cites a path-like token.
    pub has_path_citations: bool,
    /// At least one chapter has evidence/footer wording.
    pub has_evidence_footer: bool,
    /// Internal `.md` links that resolve / total internal `.md` links.
    pub links_resolved: usize,
    /// Total internal markdown link targets found.
    pub links_total: usize,
}

/// Evaluate a tutorial from an in-memory file list.
///
/// # Scoring
///
/// | Check | Weight |
/// |-------|--------|
/// | Index present | [`WEIGHT_INDEX_PRESENT`] |
/// | Index has mermaid | [`WEIGHT_INDEX_MERMAID`] |
/// | Setup or overview | [`WEIGHT_SETUP_OR_OVERVIEW`] |
/// | Mermaid validity ratio | [`WEIGHT_MERMAID_VALID`] |
/// | Path citations | [`WEIGHT_PATH_CITATIONS`] |
/// | Evidence footer | [`WEIGHT_EVIDENCE_FOOTER`] |
/// | Internal links resolve | [`WEIGHT_LINKS_RESOLVE`] |
///
/// Mermaid and link weights scale by ratio when applicable. Missing mermaid
/// blocks scores 0 for that weight (maps are required by §12.7).
///
/// # Examples
///
/// ```
/// use brigid_core::eval::{TutorialFile, evaluate_tutorial, DEFAULT_EVAL_PASS_THRESHOLD};
///
/// let files = vec![TutorialFile {
///     path: "index.md".into(),
///     content: "# T\n\n```mermaid\nflowchart LR\n  A0[Ok]\n```\n".into(),
/// }];
/// let report = evaluate_tutorial(&files, DEFAULT_EVAL_PASS_THRESHOLD);
/// assert!(report.checks.has_index);
/// assert!(report.checks.index_has_mermaid);
/// ```
#[must_use]
pub fn evaluate_tutorial(files: &[TutorialFile], threshold: i32) -> EvalReport {
    let paths: BTreeSet<String> = files.iter().map(|f| normalize_path(&f.path)).collect();
    let index = files.iter().find(|f| is_index_path(&f.path));
    let has_index = index.is_some();
    let index_has_mermaid = index.is_some_and(|f| count_mermaid_blocks(&f.content) > 0);

    let has_setup_or_overview = files
        .iter()
        .any(|f| is_setup_or_overview(&f.path, &f.content));

    let mut mermaid_block_count = 0usize;
    let mut mermaid_valid_count = 0usize;
    for f in files {
        for body in extract_mermaid_bodies(&f.content) {
            mermaid_block_count += 1;
            let sanitized = sanitize_mermaid(&body);
            if validate_mermaid(&sanitized).valid {
                mermaid_valid_count += 1;
            }
        }
    }

    let chapter_files: Vec<&TutorialFile> =
        files.iter().filter(|f| !is_index_path(&f.path)).collect();
    let has_path_citations = chapter_files.iter().any(|f| has_path_citation(&f.content));
    let has_evidence_footer = chapter_files
        .iter()
        .any(|f| has_evidence_signal(&f.content));

    let mut links_total = 0usize;
    let mut links_resolved = 0usize;
    for f in files {
        for target in extract_internal_md_links(&f.content) {
            links_total += 1;
            let resolved = resolve_link(&f.path, &target);
            if paths.contains(&resolved) {
                links_resolved += 1;
            }
        }
    }

    let checks = EvalChecks {
        has_index,
        index_has_mermaid,
        has_setup_or_overview,
        mermaid_block_count,
        mermaid_valid_count,
        has_path_citations,
        has_evidence_footer,
        links_resolved,
        links_total,
    };

    let mut score = 0i32;
    let mut reasons = Vec::new();

    if has_index {
        score += WEIGHT_INDEX_PRESENT;
    } else {
        reasons.push("missing index.md".into());
    }
    if index_has_mermaid {
        score += WEIGHT_INDEX_MERMAID;
    } else {
        reasons.push("index lacks mermaid maps".into());
    }
    if has_setup_or_overview {
        score += WEIGHT_SETUP_OR_OVERVIEW;
    } else {
        reasons.push("no setup/overview chapter".into());
    }

    if mermaid_block_count == 0 {
        reasons.push("no mermaid blocks in tutorial".into());
    } else {
        let ratio = mermaid_valid_count as f64 / mermaid_block_count as f64;
        score += (WEIGHT_MERMAID_VALID as f64 * ratio).round() as i32;
        if mermaid_valid_count < mermaid_block_count {
            reasons.push(format!(
                "invalid mermaid blocks: {mermaid_valid_count}/{mermaid_block_count} valid"
            ));
        }
    }

    if has_path_citations {
        score += WEIGHT_PATH_CITATIONS;
    } else {
        reasons.push("chapters lack path citations".into());
    }
    if has_evidence_footer {
        score += WEIGHT_EVIDENCE_FOOTER;
    } else {
        reasons.push("missing evidence/footer signals".into());
    }

    if links_total == 0 {
        // No links is neutral-ish: award half weight only when there is an index with chapters
        // Prefer requiring links from index when chapters exist.
        if chapter_files.is_empty() {
            score += WEIGHT_LINKS_RESOLVE;
        } else {
            reasons.push("no internal markdown links found".into());
        }
    } else {
        let ratio = links_resolved as f64 / links_total as f64;
        score += (WEIGHT_LINKS_RESOLVE as f64 * ratio).round() as i32;
        if links_resolved < links_total {
            reasons.push(format!(
                "broken internal links: {links_resolved}/{links_total} resolve"
            ));
        }
    }

    score = score.clamp(0, 100);
    EvalReport {
        score,
        passed: score >= threshold,
        threshold,
        checks,
        reasons,
    }
}

fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    // Collapse `.` / `..` so link targets and inventory keys match.
    let mut stack: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            p => stack.push(p),
        }
    }
    stack.join("/")
}

fn is_index_path(path: &str) -> bool {
    let p = normalize_path(path);
    let name = p.rsplit('/').next().unwrap_or(&p);
    name.eq_ignore_ascii_case("index.md")
}

fn is_setup_or_overview(path: &str, content: &str) -> bool {
    let p = normalize_path(path).to_ascii_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    if name.contains("setup") || name.contains("overview") {
        return true;
    }
    let first_heading = content.lines().find(|l| l.starts_with("# "));
    first_heading.is_some_and(|h| {
        let t = h.trim_start_matches('#').trim().to_ascii_lowercase();
        t.contains("setup") || t.contains("overview")
    })
}

fn count_mermaid_blocks(content: &str) -> usize {
    extract_mermaid_bodies(content).len()
}

fn extract_mermaid_bodies(content: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut rest = content;
    let open = "```mermaid";
    let close = "```";
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        let after = match after.find('\n') {
            Some(i) => &after[i + 1..],
            None => break,
        };
        if let Some(end) = after.find(close) {
            bodies.push(after[..end].to_owned());
            rest = &after[end + close.len()..];
        } else {
            break;
        }
    }
    bodies
}

fn has_path_citation(content: &str) -> bool {
    // Backtick path-like tokens: foo/bar.ext or bare path-ish list items.
    let mut in_tick = false;
    let mut cur = String::new();
    for ch in content.chars() {
        if ch == '`' {
            if in_tick {
                if looks_like_repo_path(&cur) {
                    return true;
                }
                cur.clear();
            }
            in_tick = !in_tick;
            continue;
        }
        if in_tick {
            cur.push(ch);
        }
    }
    // Also bare Key files sections with path-like lines
    content.lines().any(|l| {
        let t = l.trim().trim_start_matches('-').trim().trim_matches('`');
        looks_like_repo_path(t)
    })
}

fn looks_like_repo_path(s: &str) -> bool {
    if s.is_empty() || s.contains(' ') {
        return false;
    }
    if s.contains("://") {
        return false;
    }
    let has_slash = s.contains('/');
    let has_dot_ext = s.rsplit('/').next().is_some_and(|n| n.contains('.'));
    (has_slash || has_dot_ext)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
        && !s.ends_with('.')
}

fn has_evidence_signal(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("evidence")
        || lower.contains("key files")
        || lower.contains("grounded")
        || lower.contains("where this was inferred")
        || lower.contains("paths cited")
}

fn extract_internal_md_links(content: &str) -> Vec<String> {
    // [text](target.md) or [text](./foo.md#anchor)
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(mid) = content[i..].find("](") {
                let start = i + mid + 2;
                if let Some(end_rel) = content[start..].find(')') {
                    let target = &content[start..start + end_rel];
                    let target = target.split('#').next().unwrap_or(target).trim();
                    if target.ends_with(".md") && !target.contains("://") {
                        out.push(target.to_owned());
                    }
                    i = start + end_rel + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn resolve_link(from_path: &str, target: &str) -> String {
    let from = normalize_path(from_path);
    let target = normalize_path(target);
    if target.starts_with('/') {
        return target.trim_start_matches('/').to_owned();
    }
    let base = match from.rfind('/') {
        Some(i) => &from[..i],
        None => "",
    };
    let mut stack: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            p => stack.push(p),
        }
    }
    stack.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> TutorialFile {
        TutorialFile {
            path: path.into(),
            content: content.into(),
        }
    }

    #[test]
    fn good_mini_fixture_passes() {
        // Load from repo fixtures when available; fall back to inline sample.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/tutorials/good-mini");
        let files = load_dir(&root);
        assert!(!files.is_empty(), "good-mini fixtures missing");
        let report = evaluate_tutorial(&files, DEFAULT_EVAL_PASS_THRESHOLD);
        assert!(
            report.passed,
            "score={} reasons={:?}",
            report.score, report.reasons
        );
        assert!(report.score >= DEFAULT_EVAL_PASS_THRESHOLD);
    }

    #[test]
    fn broken_mini_fixture_fails() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/tutorials/broken-mini");
        let files = load_dir(&root);
        assert!(!files.is_empty(), "broken-mini fixtures missing");
        let report = evaluate_tutorial(&files, DEFAULT_EVAL_PASS_THRESHOLD);
        assert!(!report.passed, "score={} should fail", report.score);
        assert!(!report.reasons.is_empty());
    }

    #[test]
    fn empty_tree_fails() {
        let report = evaluate_tutorial(&[], DEFAULT_EVAL_PASS_THRESHOLD);
        assert!(!report.passed);
        assert!(!report.checks.has_index);
    }

    #[test]
    fn link_resolution_relative() {
        let files = vec![
            file(
                "index.md",
                "# I\n\n```mermaid\nflowchart LR\n  A0[x]\n```\n\n[C](ch/a.md)\n",
            ),
            file("ch/a.md", "# Setup\n\n`src/main.rs`\n\n## Evidence\n\nok\n"),
        ];
        let report = evaluate_tutorial(&files, DEFAULT_EVAL_PASS_THRESHOLD);
        assert_eq!(report.checks.links_total, 1);
        assert_eq!(report.checks.links_resolved, 1);
    }

    #[test]
    fn tutorial_markdown_secrets_are_redacted_on_load() {
        let dir = std::env::temp_dir().join(format!(
            "brigid-core-eval-redact-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let content = "# Tutorial\n\nDB_KEY=dummyvalue\n";
        std::fs::write(dir.join("index.md"), content).unwrap();

        let files = load_dir(&dir);
        assert_eq!(files.len(), 1);
        assert!(
            files[0].content.contains("DB_KEY=****"),
            "secret should be redacted: {}",
            files[0].content
        );
        assert!(
            !files[0].content.contains("dummyvalue"),
            "raw secret must not survive redaction"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn load_dir(root: &std::path::Path) -> Vec<TutorialFile> {
        let mut out = Vec::new();
        if !root.is_dir() {
            return out;
        }
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<TutorialFile>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Ok(content) = std::fs::read_to_string(&p) {
                        let content = crate::redact_content(&content);
                        let rel = p
                            .strip_prefix(root)
                            .map(|r| r.to_string_lossy().replace('\\', "/"))
                            .unwrap_or_default();
                        out.push(TutorialFile { path: rel, content });
                    }
                }
            }
        }
        walk(root, root, &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    use std::path::PathBuf;
}
