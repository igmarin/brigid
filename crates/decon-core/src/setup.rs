//! Setup documentation assessment (onboarding score).
//!
//! Scores README and config inventory against five signals used to decide
//! whether a generated Setup chapter is needed. Heuristics match the frozen
//! parity baseline (`tests/fixtures/baseline.json`) and
//! `docs/best-practices.md` §8.
//!
//! This module is pure: callers inject README text and a relative file list.
//! No filesystem I/O is performed here.

/// Config basenames recognised anywhere in the tree (path suffix / file name).
///
/// Only the final path component is matched, so `apps/alpha/mix.exs` counts.
pub const CONFIG_FILE_NAMES: &[&str] = &[
    ".env.example",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "mix.exs",
    "package.json",
    "Gemfile",
    "Gemfile.lock",
    "Dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "Makefile",
];

/// Minimum README length in **bytes** to avoid the short-README penalty.
pub const MIN_README_LEN: usize = 300;

/// Points awarded for each of the five setup signals when present.
pub const SIGNAL_POINTS: i32 = 20;

/// Penalty applied when a README exists but is shorter than [`MIN_README_LEN`].
pub const SHORT_README_PENALTY: i32 = 2;

/// Boolean signals detected from README text and the file inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupSignals {
    /// README content is non-empty.
    pub has_readme: bool,
    /// Install/bootstrap wording found in the README.
    pub has_install_commands: bool,
    /// Env docs via `.env.example` in the file list or env wording in README.
    pub has_env_docs: bool,
    /// Local run wording found in the README.
    pub has_run_instructions: bool,
    /// Prerequisites / runtime wording found in the README.
    pub has_prerequisites: bool,
    /// README length in bytes (`readme.len()`).
    pub readme_length: usize,
}

/// Result of assessing setup documentation quality.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupAssessment {
    /// Whether a Setup guide should be generated.
    ///
    /// `true` when `score < 80` or there are two or more gaps.
    pub needs_setup_guide: bool,
    /// Total score (typically 0–100; short README can reduce by
    /// [`SHORT_README_PENALTY`]).
    pub score: i32,
    /// Individual detection signals.
    pub signals: SetupSignals,
    /// Human-readable gap messages for missing/weak signals.
    pub gaps: Vec<String>,
    /// Relative paths whose basename is in [`CONFIG_FILE_NAMES`], sorted.
    pub config_files: Vec<String>,
}

/// Assess setup documentation from README content and a relative file list.
///
/// # Scoring
///
/// Each of the five signals contributes [`SIGNAL_POINTS`] (20) when present.
/// A non-empty README shorter than [`MIN_README_LEN`] still earns the README
/// points but applies [`SHORT_README_PENALTY`] and records a gap.
///
/// # Examples
///
/// ```
/// use decon_core::setup::assess_setup;
///
/// let readme = "\
/// # Lib\n\n## Prerequisites\n\nRequires Rust 1.85.\n\n\
/// ## Install\n\n```bash\ncargo install --path .\n```\n\n\
/// ## Environment\n\nCopy `.env.example`.\n\n\
/// ## Run\n\n```bash\ncargo run\n```\n";
/// // Pad so length is not short-penalized in this example.
/// let readme = format!("{readme}{}", "x".repeat(200));
/// let files = [".env.example", "Cargo.toml", "src/main.rs"];
/// let assessment = assess_setup(&readme, files.iter().copied());
/// assert_eq!(assessment.score, 100);
/// assert!(!assessment.needs_setup_guide);
/// assert!(assessment.gaps.is_empty());
/// ```
#[must_use]
pub fn assess_setup<I, S>(readme: &str, files: I) -> SetupAssessment
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let files: Vec<String> = files.into_iter().map(|f| f.as_ref().to_owned()).collect();
    // Normalize CRLF to LF so that README length is consistent across
    // platforms (Windows git may check out files with CRLF line endings,
    // which would inflate `readme.len()` and break baseline assertions).
    let readme_normalized = readme.replace("\r\n", "\n").replace('\r', "\n");
    let readme_lower = readme_normalized.to_lowercase();
    let readme_length = readme_normalized.len();
    let has_readme = readme_length > 0;

    let has_install_commands = readme_lower.contains("install")
        || readme_lower.contains("deps.get")
        || readme_lower.contains("bundle")
        || readme_lower.contains("docker compose");

    // Match by basename so nested paths like `docs/.env.example` count
    // (consistent with config_files discovery).
    let has_env_docs = files.iter().any(|f| file_name(f) == ".env.example")
        || readme_lower.contains(".env")
        || readme_lower.contains("environment");

    let has_run_instructions = readme_lower.contains("run")
        || readme_lower.contains("start")
        || readme_lower.contains("serve");

    let has_prerequisites =
        readme_lower.contains("prerequisite") || readme_lower.contains("require");

    let mut score: i32 = 0;
    let mut gaps: Vec<String> = Vec::new();

    if has_readme {
        score += SIGNAL_POINTS;
        if readme_length < MIN_README_LEN {
            score -= SHORT_README_PENALTY;
            gaps.push(gap_readme_missing_or_short().to_owned());
        }
    } else {
        gaps.push(gap_readme_missing_or_short().to_owned());
    }

    if has_install_commands {
        score += SIGNAL_POINTS;
    } else {
        gaps.push(gap_no_install().to_owned());
    }

    if has_env_docs {
        score += SIGNAL_POINTS;
    } else {
        gaps.push(gap_no_env().to_owned());
    }

    if has_run_instructions {
        score += SIGNAL_POINTS;
    } else {
        gaps.push(gap_no_run().to_owned());
    }

    if has_prerequisites {
        score += SIGNAL_POINTS;
    } else {
        gaps.push(gap_no_prereq().to_owned());
    }

    let needs_setup_guide = score < 80 || gaps.len() >= 2;

    let mut config_files: Vec<String> = files
        .into_iter()
        .filter(|f| {
            let name = file_name(f);
            CONFIG_FILE_NAMES.contains(&name)
        })
        .collect();
    config_files.sort();

    SetupAssessment {
        needs_setup_guide,
        score,
        signals: SetupSignals {
            has_readme,
            has_install_commands,
            has_env_docs,
            has_run_instructions,
            has_prerequisites,
            readme_length,
        },
        gaps,
        config_files,
    }
}

/// Final path component, accepting `/` or `\` separators.
///
/// Crawl normalizes to `/`; accepting `\` keeps helpers robust if a caller
/// passes OS-native paths.
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn gap_readme_missing_or_short() -> &'static str {
    "README missing or too short to onboard a newcomer"
}

fn gap_no_install() -> &'static str {
    "No install/bootstrap commands documented"
}

fn gap_no_env() -> &'static str {
    "No environment variable documentation found"
}

fn gap_no_run() -> &'static str {
    "No local run instructions documented"
}

fn gap_no_prereq() -> &'static str {
    "No prerequisites or runtime versions documented"
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time inclusion of shared workspace fixtures (no runtime I/O).
    // Paths are relative to this source file (`crates/decon-core/src/setup.rs`).
    const PYTHON_LIB_README: &str = include_str!("../../../tests/fixtures/python-lib/README.md");
    const UMBRELLA_README: &str = include_str!("../../../tests/fixtures/umbrella/README.md");
    const JS_LIB_README: &str = include_str!("../../../tests/fixtures/js-lib/README.md");

    #[test]
    fn assess_setup_python_lib_matches_baseline() {
        let files = [
            ".env.example",
            "README.md",
            "pyproject.toml",
            "src/mylib/__init__.py",
            "src/mylib/cli.py",
            "src/mylib/core.py",
        ];
        let a = assess_setup(PYTHON_LIB_README, files.iter().copied());
        assert!(!a.needs_setup_guide);
        assert_eq!(a.score, 100);
        assert!(a.signals.has_readme);
        assert!(a.signals.has_install_commands);
        assert!(a.signals.has_env_docs);
        assert!(a.signals.has_run_instructions);
        assert!(a.signals.has_prerequisites);
        // Exact length is the baseline contract; if fixtures change, update baseline.json.
        assert_eq!(a.signals.readme_length, 504);
        assert!(a.gaps.is_empty());
        assert_eq!(
            a.config_files,
            vec![".env.example".to_owned(), "pyproject.toml".to_owned()]
        );
    }

    #[test]
    fn assess_setup_umbrella_matches_baseline() {
        let files = [
            ".env.example",
            "README.md",
            "apps/alpha/lib/alpha.ex",
            "apps/alpha/mix.exs",
            "apps/beta/lib/beta.ex",
            "apps/beta/mix.exs",
            "apps/gamma/lib/gamma.ex",
            "apps/gamma/mix.exs",
            "config/config.exs",
            "mix.exs",
        ];
        let a = assess_setup(UMBRELLA_README, files.iter().copied());
        assert!(!a.needs_setup_guide);
        assert_eq!(a.score, 100);
        assert_eq!(a.signals.readme_length, 543);
        assert!(a.gaps.is_empty());
        assert_eq!(
            a.config_files,
            vec![
                ".env.example".to_owned(),
                "apps/alpha/mix.exs".to_owned(),
                "apps/beta/mix.exs".to_owned(),
                "apps/gamma/mix.exs".to_owned(),
                "mix.exs".to_owned(),
            ]
        );
    }

    #[test]
    fn assess_setup_js_lib_matches_baseline() {
        let files = [
            ".env.example",
            "README.md",
            "package.json",
            "src/index.ts",
            "src/utils.ts",
            "tsconfig.json",
        ];
        let a = assess_setup(JS_LIB_README, files.iter().copied());
        assert!(!a.needs_setup_guide);
        assert_eq!(a.score, 100);
        assert_eq!(a.signals.readme_length, 383);
        assert!(a.gaps.is_empty());
        assert_eq!(
            a.config_files,
            vec![".env.example".to_owned(), "package.json".to_owned()]
        );
    }

    #[test]
    fn env_docs_from_nested_env_example_basename() {
        // Important: nested `.env.example` must count like config_files do.
        let readme = "x".repeat(MIN_README_LEN)
            + "\n## Prerequisites\nRequires Go.\n## Install\ngo install\n## Run\ngo run .\n";
        // No env keywords in README; only nested file.
        let a = assess_setup(&readme, ["docs/.env.example", "main.go"].iter().copied());
        assert!(a.signals.has_env_docs);
        assert_eq!(a.score, 100);
        assert!(a.config_files.iter().any(|f| f == "docs/.env.example"));
    }

    #[test]
    fn empty_readme_and_no_signals_scores_zero_with_gaps() {
        let a = assess_setup("", ["src/main.rs"].iter().copied());
        assert_eq!(a.score, 0);
        assert!(!a.signals.has_readme);
        assert!(!a.signals.has_install_commands);
        assert!(!a.signals.has_env_docs);
        assert!(!a.signals.has_run_instructions);
        assert!(!a.signals.has_prerequisites);
        assert!(a.needs_setup_guide);
        assert_eq!(a.gaps.len(), 5);
        assert!(a.config_files.is_empty());
    }

    #[test]
    fn short_readme_applies_penalty_and_gap() {
        // Has install + run + require keywords, no env file / wording.
        let readme = "Install me. Run me. Requires Python.\n";
        assert!(readme.len() < MIN_README_LEN);
        let a = assess_setup(readme, ["src/a.py"].iter().copied());
        assert!(a.signals.has_readme);
        assert!(a.signals.has_install_commands);
        assert!(a.signals.has_run_instructions);
        assert!(a.signals.has_prerequisites);
        assert!(!a.signals.has_env_docs);
        // 20*4 - 2 short penalty = 78 for four signals; no env → only 3 signals + readme = 4*20 - 2 = 78
        assert_eq!(a.score, 78);
        assert!(a.gaps.iter().any(|g| g.contains("too short")));
        assert!(a.gaps.iter().any(|g| g.contains("environment")));
        assert!(a.needs_setup_guide); // score < 80 and gaps >= 2
    }

    #[test]
    fn env_docs_from_env_example_file_without_readme_mention() {
        let readme = "x".repeat(MIN_README_LEN)
            + "\n## Prerequisites\nRequires Go.\n## Install\ngo install\n## Run\ngo run .\n";
        let a = assess_setup(&readme, [".env.example", "main.go"].iter().copied());
        assert!(a.signals.has_env_docs);
        assert_eq!(a.score, 100);
        assert!(!a.needs_setup_guide);
    }

    #[test]
    fn config_files_matched_by_basename_anywhere() {
        let a = assess_setup(
            "",
            [
                "apps/alpha/mix.exs",
                "docker-compose.yml",
                "src/lib.rs",
                "Makefile",
            ]
            .iter()
            .copied(),
        );
        assert_eq!(
            a.config_files,
            vec![
                "Makefile".to_owned(),
                "apps/alpha/mix.exs".to_owned(),
                "docker-compose.yml".to_owned(),
            ]
        );
    }

    #[test]
    fn file_name_helper_handles_nested_and_root() {
        assert_eq!(file_name("apps/alpha/mix.exs"), "mix.exs");
        assert_eq!(file_name("README.md"), "README.md");
        assert_eq!(file_name(".env.example"), ".env.example");
        assert_eq!(file_name(r"apps\alpha\mix.exs"), "mix.exs");
    }
}
