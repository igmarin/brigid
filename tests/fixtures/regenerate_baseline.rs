//! Standalone baseline regenerator for M1 parity fixtures.
//!
//! Pure Rust, zero dependencies. Reimplements the Python reference's
//! crawl / module-discovery / setup-assessment heuristics as documented in
//! `docs/best-practices.md`, so that `baseline.json` can be regenerated or
//! verified without a Python toolchain.
//!
//! The frozen `baseline.json` remains the independent oracle — this tool must
//! *match* it. When `brigid-crawl` is built in M1, it is tested against the same
//! frozen baseline, keeping the parity test non-circular.
//!
//! ## Usage
//!
//! ```text
//! rustc tests/fixtures/regenerate_baseline.rs -o /tmp/regen_baseline
//! /tmp/regen_baseline tests/fixtures/ --check    # verify baseline.json matches
//! /tmp/regen_baseline tests/fixtures/ --write    # overwrite baseline.json
//! ```
//!
//! Exit codes: 0 = success, 1 = mismatch (check mode), 2 = usage error.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Config file names the reference tooling recognises.
///
/// Only the filename is matched (not the full path), so `apps/alpha/mix.exs`
/// is included for umbrella fixtures.
const CONFIG_FILE_NAMES: &[&str] = &[
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

/// Minimum README length (bytes) to avoid the "too short" penalty.
const MIN_README_LEN: usize = 300;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

struct CrawlResult {
    file_count: usize,
    files: Vec<String>,
}

struct FilterStats {
    filtered: bool,
    before: usize,
    after: usize,
    kept_shared: usize,
    module_keys: Vec<String>,
}

/// A scoped filter run: filter the file set to specific modules plus shared
/// root scaffolding, recording the resulting stats.
struct ScopedRun {
    scope: Vec<String>,
    /// Relative paths retained after filtering (crawl order preserved).
    files: Vec<String>,
    filter_stats: FilterStats,
}

struct SetupSignals {
    has_readme: bool,
    has_install_commands: bool,
    has_env_docs: bool,
    has_run_instructions: bool,
    has_prerequisites: bool,
    readme_length: usize,
}

struct SetupAssessment {
    needs_setup_guide: bool,
    score: i32,
    signals: SetupSignals,
    gaps: Vec<String>,
    config_files: Vec<String>,
}

struct FixtureBaseline {
    crawl: CrawlResult,
    modules: Vec<(String, usize)>,
    filter_stats: FilterStats,
    scoped_runs: Vec<ScopedRun>,
    setup_assessment: SetupAssessment,
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

/// Recursively walk a directory and collect all file paths (relative to the
/// fixture root), skipping hidden directories (`.git`, `.venv`, …) but
/// including hidden files (`.env.example`).
fn crawl_local_files(root: &Path) -> CrawlResult {
    let mut files: Vec<String> = Vec::new();
    crawl_dir(root, root, &mut files);
    files.sort();
    CrawlResult {
        file_count: files.len(),
        files,
    }
}

fn crawl_dir(root: &Path, dir: &Path, files: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden directories, allow hidden files.
        if path.is_dir() && name_str.starts_with('.') {
            continue;
        }

        if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
        } else if path.is_dir() {
            crawl_dir(root, &path, files);
        }
    }
}

/// Determine the module key for a file path.
///
/// - Root-level files → `_root`
/// - `apps/<name>/…` (3+ components) → `apps/<name>` (umbrella two-level key)
/// - Files directly under `apps/` (e.g. `apps/README.md`) → `apps`
/// - `<dir>/…` → `<dir>` (top-level directory name)
fn module_key(path: &str) -> String {
    let components: Vec<&str> = path.split('/').collect();
    if components.len() == 1 {
        "_root".to_string()
    } else if components[0] == "apps" && components.len() >= 3 {
        format!("apps/{}", components[1])
    } else if components[0] == "apps" {
        "apps".to_string()
    } else {
        components[0].to_string()
    }
}

/// Sort priority for module keys: `apps/*` first, then `_root`, then
/// everything else alphabetically. This matches the Python reference's
/// module ordering.
fn module_sort_key(key: &str) -> (u8, String) {
    if key.starts_with("apps/") {
        (0, key.to_string())
    } else if key == "_root" {
        (1, String::new())
    } else {
        (2, key.to_string())
    }
}

/// Group files into modules and count per module, ordered with the
/// reference's custom sort (apps/* → _root → others).
fn discover_modules(files: &[String]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        *counts.entry(module_key(file)).or_insert(0) += 1;
    }
    let mut modules: Vec<(String, usize)> = counts.into_iter().collect();
    modules.sort_by(|a, b| module_sort_key(&a.0).cmp(&module_sort_key(&b.0)));
    modules
}

/// Compute filter stats when no scope filter is applied.
fn filter_stats(files: &[String], modules: &[(String, usize)]) -> FilterStats {
    let module_keys: Vec<String> = modules.iter().map(|(k, _)| k.clone()).collect();
    FilterStats {
        filtered: false,
        before: files.len(),
        after: files.len(),
        kept_shared: 0,
        module_keys,
    }
}

/// Determine whether a module key is "shared" (root scaffolding kept when
/// scoping to specific apps). Per `docs/best-practices.md` §2.3: root-level
/// files and `config/` are shared scaffolding.
fn is_shared_module(key: &str) -> bool {
    key == "_root" || key == "config"
}

/// Filter files to those belonging to the selected modules plus shared root
/// scaffolding. Returns the filtered file list and filter stats.
fn filter_files_by_scope(files: &[String], scope: &[String]) -> ScopedRun {
    let scope_set: BTreeMap<String, ()> = scope.iter().map(|s| (s.clone(), ())).collect();

    let mut after_files: Vec<String> = Vec::new();
    let mut kept_shared = 0;

    for file in files {
        let key = module_key(file);
        let in_scope = scope_set.contains_key(&key);
        let is_shared = is_shared_module(&key);
        if in_scope || is_shared {
            after_files.push(file.clone());
            if is_shared && !in_scope {
                kept_shared += 1;
            }
        }
    }

    let mut module_keys: Vec<String> = scope.to_vec();
    module_keys.sort_by(|a, b| module_sort_key(a).cmp(&module_sort_key(b)));

    let after = after_files.len();
    ScopedRun {
        scope: scope.to_vec(),
        files: after_files,
        filter_stats: FilterStats {
            filtered: true,
            before: files.len(),
            after,
            kept_shared,
            module_keys,
        },
    }
}

/// Generate deterministic scoped runs for a fixture. Only the umbrella
/// fixture has multiple apps, so only it gets scoped cases.
fn generate_scoped_runs(fixture_name: &str, files: &[String]) -> Vec<ScopedRun> {
    if fixture_name != "umbrella" {
        return Vec::new();
    }

    vec![
        filter_files_by_scope(files, &["apps/alpha".to_string()]),
        filter_files_by_scope(files, &["apps/alpha".to_string(), "apps/beta".to_string()]),
    ]
}

/// Assess setup documentation quality based on README content and config files.
fn assess_setup(root: &Path, files: &[String]) -> SetupAssessment {
    let readme_path = root.join("README.md");
    let readme_content = fs::read_to_string(&readme_path).unwrap_or_default();
    let readme_lower = readme_content.to_lowercase();
    let readme_length = readme_content.len();

    let has_readme = readme_length > 0;

    // Install / bootstrap commands.
    let has_install_commands = readme_lower.contains("install")
        || readme_lower.contains("deps.get")
        || readme_lower.contains("bundle")
        || readme_lower.contains("docker compose");

    // Environment variable documentation (basename match for nested paths).
    let has_env_docs = files.iter().any(|f| {
        Path::new(f)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == ".env.example")
    }) || readme_lower.contains(".env")
        || readme_lower.contains("environment");

    // How to run locally.
    let has_run_instructions = readme_lower.contains("run")
        || readme_lower.contains("start")
        || readme_lower.contains("serve");

    // Prerequisites / language runtime versions.
    let has_prerequisites = readme_lower.contains("prerequisite")
        || readme_lower.contains("require");

    // Score: each of the 5 signals contributes 20 points (max 100).
    // A short README (< MIN_README_LEN) gets a small penalty on top.
    let mut score: i32 = 0;
    let mut gaps: Vec<String> = Vec::new();

    if has_readme {
        score += 20;
        if readme_length < MIN_README_LEN {
            score -= 2;
            gaps.push("README missing or too short to onboard a newcomer".to_string());
        }
    } else {
        gaps.push("README missing or too short to onboard a newcomer".to_string());
    }

    if has_install_commands {
        score += 20;
    } else {
        gaps.push("No install/bootstrap commands documented".to_string());
    }

    if has_env_docs {
        score += 20;
    } else {
        gaps.push("No environment variable documentation found".to_string());
    }

    if has_run_instructions {
        score += 20;
    } else {
        gaps.push("No local run instructions documented".to_string());
    }

    if has_prerequisites {
        score += 20;
    } else {
        gaps.push("No prerequisites or runtime versions documented".to_string());
    }

    let needs_setup_guide = score < 80 || gaps.len() >= 2;

    // Config files: match by filename anywhere in the tree.
    let mut config_files: Vec<String> = files
        .iter()
        .filter(|f| {
            let name = Path::new(f)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            CONFIG_FILE_NAMES.contains(&name)
        })
        .cloned()
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

fn generate_fixture_baseline(name: &str, root: &Path) -> FixtureBaseline {
    let crawl = crawl_local_files(root);
    let modules = discover_modules(&crawl.files);
    let filter_stats = filter_stats(&crawl.files, &modules);
    let scoped_runs = generate_scoped_runs(name, &crawl.files);
    let setup_assessment = assess_setup(root, &crawl.files);
    FixtureBaseline {
        crawl,
        modules,
        filter_stats,
        scoped_runs,
        setup_assessment,
    }
}

// ---------------------------------------------------------------------------
// JSON emission (matches the existing baseline.json formatting exactly)
// ---------------------------------------------------------------------------

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn emit_string_array(items: &[String], indent_level: usize) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let mut s = String::from("[\n");
    for (i, item) in items.iter().enumerate() {
        s.push_str(&"  ".repeat(indent_level + 1));
        s.push_str(&format!("\"{}\"", escape_json(item)));
        if i < items.len() - 1 {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str(&"  ".repeat(indent_level));
    s.push(']');
    s
}

fn emit_fixture(name: &str, fb: &FixtureBaseline) -> String {
    let mut s = String::new();
    let ind = "  ".to_string();

    // Fixture key
    s.push_str(&format!("{ind}\"{}\": {{\n", escape_json(name)));

    // crawl
    s.push_str(&format!("{ind}{ind}\"crawl\": {{\n"));
    s.push_str(&format!(
        "{ind}{ind}{ind}\"file_count\": {},\n",
        fb.crawl.file_count
    ));
    s.push_str(&format!("{ind}{ind}{ind}\"files\": "));
    s.push_str(&emit_string_array(&fb.crawl.files, 3));
    s.push_str(&format!("\n{ind}{ind}}},\n"));

    // modules
    s.push_str(&format!("{ind}{ind}\"modules\": {{\n"));
    for (i, (key, count)) in fb.modules.iter().enumerate() {
        s.push_str(&format!("{ind}{ind}{ind}\"{}\": {count}", escape_json(key)));
        if i < fb.modules.len() - 1 {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str(&format!("{ind}{ind}}},\n"));

    // filter_stats
    s.push_str(&format!("{ind}{ind}\"filter_stats\": {{\n"));
    s.push_str(&format!(
        "{ind}{ind}{ind}\"filtered\": {},\n",
        fb.filter_stats.filtered
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}\"before\": {},\n",
        fb.filter_stats.before
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}\"after\": {},\n",
        fb.filter_stats.after
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}\"kept_shared\": {},\n",
        fb.filter_stats.kept_shared
    ));
    s.push_str(&format!("{ind}{ind}{ind}\"module_keys\": "));
    s.push_str(&emit_string_array(&fb.filter_stats.module_keys, 3));
    s.push_str(&format!("\n{ind}{ind}}},\n"));

    // scoped_runs
    s.push_str(&format!("{ind}{ind}\"scoped_runs\": "));
    if fb.scoped_runs.is_empty() {
        s.push_str("[],\n");
    } else {
        s.push_str("[\n");
        for (i, run) in fb.scoped_runs.iter().enumerate() {
            s.push_str(&format!("{ind}{ind}{ind}{{\n"));
            s.push_str(&format!("{ind}{ind}{ind}{ind}\"scope\": "));
            s.push_str(&emit_string_array(&run.scope, 4));
            s.push_str(&format!(",\n"));
            s.push_str(&format!("{ind}{ind}{ind}{ind}\"files\": "));
            s.push_str(&emit_string_array(&run.files, 4));
            s.push_str(&format!(",\n"));
            s.push_str(&format!("{ind}{ind}{ind}{ind}\"filter_stats\": {{\n"));
            s.push_str(&format!(
                "{ind}{ind}{ind}{ind}{ind}\"filtered\": {},\n",
                run.filter_stats.filtered
            ));
            s.push_str(&format!(
                "{ind}{ind}{ind}{ind}{ind}\"before\": {},\n",
                run.filter_stats.before
            ));
            s.push_str(&format!(
                "{ind}{ind}{ind}{ind}{ind}\"after\": {},\n",
                run.filter_stats.after
            ));
            s.push_str(&format!(
                "{ind}{ind}{ind}{ind}{ind}\"kept_shared\": {},\n",
                run.filter_stats.kept_shared
            ));
            s.push_str(&format!("{ind}{ind}{ind}{ind}{ind}\"module_keys\": "));
            s.push_str(&emit_string_array(&run.filter_stats.module_keys, 5));
            s.push_str(&format!("\n{ind}{ind}{ind}{ind}}}\n"));
            s.push_str(&format!("{ind}{ind}{ind}}}"));
            if i < fb.scoped_runs.len() - 1 {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str(&format!("{ind}{ind}],\n"));
    }

    // setup_assessment
    s.push_str(&format!("{ind}{ind}\"setup_assessment\": {{\n"));
    s.push_str(&format!(
        "{ind}{ind}{ind}\"needs_setup_guide\": {},\n",
        fb.setup_assessment.needs_setup_guide
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}\"score\": {},\n",
        fb.setup_assessment.score
    ));

    // signals
    s.push_str(&format!("{ind}{ind}{ind}\"signals\": {{\n"));
    s.push_str(&format!(
        "{ind}{ind}{ind}{ind}\"has_readme\": {},\n",
        fb.setup_assessment.signals.has_readme
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}{ind}\"has_install_commands\": {},\n",
        fb.setup_assessment.signals.has_install_commands
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}{ind}\"has_env_docs\": {},\n",
        fb.setup_assessment.signals.has_env_docs
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}{ind}\"has_run_instructions\": {},\n",
        fb.setup_assessment.signals.has_run_instructions
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}{ind}\"has_prerequisites\": {},\n",
        fb.setup_assessment.signals.has_prerequisites
    ));
    s.push_str(&format!(
        "{ind}{ind}{ind}{ind}\"readme_length\": {}\n",
        fb.setup_assessment.signals.readme_length
    ));
    s.push_str(&format!("{ind}{ind}{ind}}},\n"));

    // gaps
    s.push_str(&format!("{ind}{ind}{ind}\"gaps\": "));
    s.push_str(&emit_string_array(&fb.setup_assessment.gaps, 3));
    s.push_str(&format!(",\n"));

    // config_files
    s.push_str(&format!("{ind}{ind}{ind}\"config_files\": "));
    s.push_str(&emit_string_array(&fb.setup_assessment.config_files, 3));
    s.push_str(&format!("\n{ind}{ind}}}\n"));

    // close fixture
    s.push_str(&format!("{ind}}}"));

    s
}

fn emit_baseline(fixtures: &[(String, FixtureBaseline)]) -> String {
    let mut s = String::from("{\n");
    for (i, (name, fb)) in fixtures.iter().enumerate() {
        s.push_str(&emit_fixture(name, fb));
        if i < fixtures.len() - 1 {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("}\n");
    s
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <fixtures_dir> [--check|--write]", args[0]);
        eprintln!("  --check  Compare generated baseline against baseline.json (default)");
        eprintln!("  --write  Overwrite baseline.json with generated output");
        return ExitCode::from(2);
    }

    let fixtures_dir = PathBuf::from(&args[1]);
    let mode = args.get(2).map(|s| s.as_str()).unwrap_or("--check");
    let write_mode = match mode {
        "--write" => true,
        "--check" => false,
        _ => {
            eprintln!("Error: unknown mode '{mode}' (expected --check or --write)");
            return ExitCode::from(2);
        }
    };

    if !fixtures_dir.is_dir() {
        eprintln!("Error: {} is not a directory", fixtures_dir.display());
        return ExitCode::from(2);
    }

    // Discover fixture subdirectories. Error on non-UTF-8 names so
    // accidental encoding issues surface in CI rather than being silently
    // skipped (which would produce a misleading baseline mismatch).
    let mut found: Vec<String> = Vec::new();
    for entry in fs::read_dir(&fixtures_dir).expect("read fixtures dir") {
        let entry = entry.expect("read dir entry");
        if entry.path().is_dir() {
            let name = entry.file_name().into_string().unwrap_or_else(|name| {
                eprintln!(
                    "Error: fixture directory name is not valid UTF-8: {name:?}",
                );
                std::process::exit(2);
            });
            found.push(name);
        }
    }

    // Crawl/setup parity fixtures only (issue #7). Other dirs under
    // tests/fixtures/ (e.g. tutorials/ for structural eval) are intentionally
    // excluded from baseline.json.
    let canonical = ["python-lib", "umbrella", "js-lib"];
    let mut fixture_names: Vec<String> = Vec::new();
    for name in &canonical {
        if found.iter().any(|f| f == name) {
            fixture_names.push(name.to_string());
        } else {
            eprintln!("Error: required fixture directory missing: {name}");
            std::process::exit(2);
        }
    }

    // Generate baselines for each fixture (preserving canonical order).
    let mut baselines: Vec<(String, FixtureBaseline)> = Vec::new();
    for name in &fixture_names {
        let fixture_path = fixtures_dir.join(name);
        baselines.push((name.clone(), generate_fixture_baseline(name, &fixture_path)));
    }

    let generated = emit_baseline(&baselines);
    let baseline_path = fixtures_dir.join("baseline.json");

    if write_mode {
        fs::write(&baseline_path, &generated).expect("write baseline.json");
        eprintln!("Wrote {}", baseline_path.display());
        ExitCode::SUCCESS
    } else {
        let existing = fs::read_to_string(&baseline_path).unwrap_or_default();
        if existing == generated {
            eprintln!("baseline.json matches fixtures ({} fixtures checked)", fixture_names.len());
            ExitCode::SUCCESS
        } else {
            eprintln!("baseline.json does NOT match fixtures!");
            eprintln!();
            // Show a compact diff of the first differing line.
            let gen_lines: Vec<&str> = generated.lines().collect();
            let old_lines: Vec<&str> = existing.lines().collect();
            let max = gen_lines.len().max(old_lines.len());
            for i in 0..max {
                let g = gen_lines.get(i).copied().unwrap_or("<missing>");
                let o = old_lines.get(i).copied().unwrap_or("<missing>");
                if g != o {
                    eprintln!("  line {}: expected: {g}", i + 1);
                    eprintln!("  line {}: got:      {o}", i + 1);
                    break;
                }
            }
            eprintln!();
            eprintln!("Run with --write to update baseline.json, or fix the fixtures.");
            ExitCode::FAILURE
        }
    }
}
