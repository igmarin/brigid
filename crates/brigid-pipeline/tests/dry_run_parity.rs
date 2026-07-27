//! Parity: `dry_run` on fixtures matches `tests/fixtures/baseline.json`.
//!
//! Budget fields are not in the baseline; they are only smoke-checked.

#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;

use brigid_core::ModuleKey;
use brigid_pipeline::dry_run;
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn baseline() -> Value {
    let path = fixtures_dir().join("baseline.json");
    let raw = fs::read_to_string(&path).expect("read baseline.json");
    serde_json::from_str(&raw).expect("parse baseline.json")
}

fn assert_filter_stats(actual: &brigid_core::FilterStats, expected: &Value) {
    assert_eq!(
        actual.filtered,
        expected["filtered"].as_bool().expect("filtered bool")
    );
    assert_eq!(
        actual.before,
        expected["before"].as_u64().expect("before") as usize
    );
    assert_eq!(
        actual.after,
        expected["after"].as_u64().expect("after") as usize
    );
    assert_eq!(
        actual.kept_shared,
        expected["kept_shared"].as_u64().expect("kept_shared") as usize
    );
    let expected_keys: Vec<&str> = expected["module_keys"]
        .as_array()
        .expect("module_keys")
        .iter()
        .map(|v| v.as_str().expect("key str"))
        .collect();
    let actual_keys: Vec<&str> = actual.module_keys.iter().map(|k| k.as_str()).collect();
    assert_eq!(actual_keys, expected_keys);
}

fn assert_setup(actual: &brigid_core::SetupAssessment, expected: &Value) {
    assert_eq!(
        actual.needs_setup_guide,
        expected["needs_setup_guide"].as_bool().unwrap()
    );
    assert_eq!(actual.score, expected["score"].as_i64().unwrap() as i32);
    let sig = &expected["signals"];
    assert_eq!(
        actual.signals.has_readme,
        sig["has_readme"].as_bool().unwrap()
    );
    assert_eq!(
        actual.signals.has_install_commands,
        sig["has_install_commands"].as_bool().unwrap()
    );
    assert_eq!(
        actual.signals.has_env_docs,
        sig["has_env_docs"].as_bool().unwrap()
    );
    assert_eq!(
        actual.signals.has_run_instructions,
        sig["has_run_instructions"].as_bool().unwrap()
    );
    assert_eq!(
        actual.signals.has_prerequisites,
        sig["has_prerequisites"].as_bool().unwrap()
    );
    assert_eq!(
        actual.signals.readme_length,
        sig["readme_length"].as_u64().unwrap() as usize
    );
    let expected_gaps: Vec<&str> = expected["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let actual_gaps: Vec<&str> = actual.gaps.iter().map(String::as_str).collect();
    assert_eq!(actual_gaps, expected_gaps);
    let expected_cf: Vec<&str> = expected["config_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let actual_cf: Vec<&str> = actual.config_files.iter().map(String::as_str).collect();
    assert_eq!(actual_cf, expected_cf);
}

fn assert_unscoped_fixture(name: &str) {
    let base = &baseline()[name];
    let root = fixtures_dir().join(name);
    let plan = dry_run(&root, None).unwrap_or_else(|e| panic!("dry_run {name}: {e}"));

    // crawl
    let expected_files: Vec<&str> = base["crawl"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        plan.files.len(),
        base["crawl"]["file_count"].as_u64().unwrap() as usize
    );
    let actual_files: Vec<&str> = plan.files.iter().map(String::as_str).collect();
    assert_eq!(actual_files, expected_files);

    // modules (ordered inventory vs JSON object — check counts and baseline order)
    let modules_obj = base["modules"].as_object().unwrap();
    assert_eq!(plan.modules.len(), modules_obj.len());
    for m in &plan.modules {
        let expected_count = modules_obj[m.key.as_str()].as_u64().unwrap() as usize;
        assert_eq!(m.count, expected_count, "module {}", m.key.as_str());
    }
    // Order matches regenerator: apps/* then _root then others
    let keys: Vec<&str> = plan.modules.iter().map(|m| m.key.as_str()).collect();
    let expected_order: Vec<&str> = base["filter_stats"]["module_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(keys, expected_order);

    assert_filter_stats(&plan.filter_stats, &base["filter_stats"]);
    assert_setup(&plan.setup, &base["setup_assessment"]);

    // Budget smoke (not in baseline.json): sizes and batch packing.
    assert_eq!(plan.budget.file_count, plan.files.len());
    if plan.files.is_empty() {
        assert_eq!(plan.budget.raw_chars, 0);
        assert_eq!(plan.budget.batch_count, 0);
    } else {
        assert!(plan.budget.raw_chars > 0);
        assert!(plan.budget.batch_count >= 1);
    }
}

#[test]
fn dry_run_python_lib_matches_baseline() {
    assert_unscoped_fixture("python-lib");
}

#[test]
fn dry_run_umbrella_matches_baseline() {
    assert_unscoped_fixture("umbrella");
}

#[test]
fn dry_run_js_lib_matches_baseline() {
    assert_unscoped_fixture("js-lib");
}

#[test]
fn dry_run_umbrella_scoped_runs_match_baseline() {
    let base = &baseline()["umbrella"];
    let root = fixtures_dir().join("umbrella");
    let scoped = base["scoped_runs"].as_array().unwrap();

    for run in scoped {
        let scope_keys: Vec<ModuleKey> = run["scope"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| ModuleKey::new(v.as_str().unwrap()))
            .collect();
        let plan =
            dry_run(&root, Some(&scope_keys)).unwrap_or_else(|e| panic!("scoped dry_run: {e}"));
        assert_filter_stats(&plan.filter_stats, &run["filter_stats"]);
        assert_eq!(plan.files.len(), plan.filter_stats.after);
        // Exact scoped inventory (not only counts / a few exclusions).
        let expected_files: Vec<&str> = run["files"]
            .as_array()
            .expect("scoped_runs[].files")
            .iter()
            .map(|v| v.as_str().expect("file str"))
            .collect();
        let actual_files: Vec<&str> = plan.files.iter().map(String::as_str).collect();
        assert_eq!(actual_files, expected_files, "scope={:?}", scope_keys);
        // Full-repo modules inventory still reflects unscoped discovery
        assert!(plan.modules.iter().any(|m| m.key.as_str() == "apps/gamma"));
        // Scoped working set excludes out-of-scope apps
        if scope_keys.len() == 1 {
            assert!(!plan.files.iter().any(|f| f.starts_with("apps/beta")));
            assert!(!plan.files.iter().any(|f| f.starts_with("apps/gamma")));
        }
    }
}
