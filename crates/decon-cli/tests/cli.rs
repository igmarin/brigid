//! Smoke tests for the `decon` binary's argument parsing and M1 subcommands.
//!
//! These exercise process-boundary behavior (exit code, `--help`, subcommand
//! contracts). Pipeline logic is unit-tested in library crates.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn decon() -> Command {
    let mut cmd = Command::cargo_bin("decon").expect("decon binary should build");
    cmd.env("DECON_FORCE_MOCK", "1");
    cmd
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

#[test]
fn version_flag_prints_version_and_exits_zero() {
    decon()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_subcommands() {
    decon()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("crawl"))
        .stdout(predicate::str::contains("dry-run"))
        .stdout(predicate::str::contains("eval"))
        .stdout(predicate::str::contains("resume"))
        .stdout(predicate::str::contains("init"));
}

#[test]
fn unknown_flag_exits_nonzero() {
    decon().arg("--not-a-real-flag").assert().failure();
}

#[test]
fn crawl_python_lib_text() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["crawl", "--dir"])
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("files:"))
        .stdout(predicate::str::contains("README.md"));
}

#[test]
fn crawl_json_has_file_count() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["crawl", "--dir"])
        .arg(&dir)
        .args(["--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"file_count\""))
        .stdout(predicate::str::contains("\"files\""));
}

#[test]
fn dry_run_json_on_fixture() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["dry-run", "--dir"])
        .arg(&dir)
        .args(["--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"filter_stats\""))
        .stdout(predicate::str::contains("\"budget\""))
        .stdout(predicate::str::contains("\"setup\""));
}

#[test]
fn dry_run_with_apps_scope() {
    let dir = fixtures_dir().join("umbrella");
    decon()
        .args(["dry-run", "--dir"])
        .arg(&dir)
        .args(["--apps", "apps/alpha", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"filtered\":true"));
}

#[test]
fn eval_good_mini_exits_zero() {
    let dir = fixtures_dir().join("tutorials/good-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("passed=true"));
}

#[test]
fn eval_broken_mini_exits_nonzero() {
    let dir = fixtures_dir().join("tutorials/broken-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .assert()
        .failure()
        .stdout(predicate::str::contains("passed=false"));
}

#[test]
fn eval_llm_generated_fixture_passes_at_threshold_70() {
    let dir = fixtures_dir().join("tutorials/llm-generated");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .args(["--threshold", "70"])
        .assert()
        .success()
        .stdout(predicate::str::contains("passed=true"));
}

#[test]
fn init_writes_decon_toml() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("decon-cli-init-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--non-interactive"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote"));
    assert!(dir.join("decon.toml").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_missing_checkpoint_exits_config() {
    decon()
        .args(["resume", "--checkpoint", "/no/such/checkpoint-dir-decon"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn resume_valid_checkpoint_json() {
    use decon_core::{CheckpointV1, RunConfig, StageId};
    use decon_pipeline::{CheckpointStore, records_from_files};
    use std::time::{SystemTime, UNIX_EPOCH};

    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("decon-cli-resume-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
    let files = records_from_files(&[("a.txt", b"hi" as &[u8])]);
    CheckpointStore::new(&dir).save(meta, &files).unwrap();

    decon()
        .args(["resume", "--checkpoint"])
        .arg(&dir)
        .args(["--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"next_stage\""))
        .stdout(predicate::str::contains("dry_run"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Error-path & format coverage for issue #78 (raise main.rs coverage >= 80%).
//
// These are characterization tests of the existing CLI behavior: they pin
// exit codes and output shapes for the uncovered branches in main.rs
// (crawl/dry-run/eval error paths, text vs. json formats, config discovery,
// init overwrite refusal, recursive tutorial walks).
// ---------------------------------------------------------------------------

/// Build a unique temporary directory name (no `tempfile` dev-dep required).
fn temp_dir(label: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("decon-cli-{label}-{n}"))
}

#[test]
fn crawl_missing_dir_exits_config() {
    decon()
        .args(["crawl", "--dir", "/no/such/decon-crawl-dir"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("crawl failed"));
}

#[test]
fn dry_run_text_format_on_fixture() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["dry-run", "--dir"])
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("root:"))
        .stdout(predicate::str::contains("files: 6"))
        .stdout(predicate::str::contains("modules:"))
        .stdout(predicate::str::contains("filter:"))
        .stdout(predicate::str::contains("setup:"))
        .stdout(predicate::str::contains("budget:"));
}

#[test]
fn dry_run_missing_dir_exits_config() {
    decon()
        .args(["dry-run", "--dir", "/no/such/decon-dry-run-dir"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("dry-run failed"));
}

#[test]
fn eval_missing_dir_exits_config() {
    decon()
        .args(["eval", "--out", "/no/such/decon-eval-dir"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("eval failed to load tutorial"));
}

#[test]
fn eval_json_format_on_good_mini() {
    let dir = fixtures_dir().join("tutorials/good-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .args(["--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"score\""))
        .stdout(predicate::str::contains("\"passed\":true"))
        .stdout(predicate::str::contains("\"threshold\":70"))
        .stdout(predicate::str::contains("\"checks\""))
        .stdout(predicate::str::contains("\"has_index\":true"))
        .stdout(predicate::str::contains("\"mermaid_block_count\""));
}

#[test]
fn eval_json_format_on_broken_mini_fails() {
    let dir = fixtures_dir().join("tutorials/broken-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .args(["--format", "json"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("\"passed\":false"))
        .stdout(predicate::str::contains("\"reasons\""));
}

#[test]
fn eval_failing_threshold_exits_fail() {
    // good-mini scores 100; a threshold above the score forces a structural
    // eval failure (exit 1) even on a well-formed tutorial.
    let dir = fixtures_dir().join("tutorials/good-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .args(["--threshold", "101"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("passed=false"))
        .stdout(predicate::str::contains("threshold=101"));
}

#[test]
fn resume_text_format_on_valid_checkpoint() {
    use decon_core::{CheckpointV1, RunConfig, StageId};
    use decon_pipeline::{CheckpointStore, records_from_files};

    let dir = temp_dir("resume-text");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
    let files = records_from_files(&[("a.txt", b"hi" as &[u8])]);
    CheckpointStore::new(&dir).save(meta, &files).unwrap();

    decon()
        .args(["resume", "--checkpoint"])
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("checkpoint:"))
        .stdout(predicate::str::contains("version:"))
        .stdout(predicate::str::contains("source_revision:"))
        .stdout(predicate::str::contains("identity_ok:"))
        .stdout(predicate::str::contains("files_in_bundle:"))
        .stdout(predicate::str::contains("completed:"))
        .stdout(predicate::str::contains("next_stage:"))
        .stdout(predicate::str::contains("pending:"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_refuses_to_overwrite_existing_config() {
    let dir = temp_dir("init-overwrite");
    std::fs::create_dir_all(&dir).unwrap();
    // Pre-create the config so `init` must refuse.
    std::fs::write(dir.join("decon.toml"), b"# pre-existing").unwrap();

    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--non-interactive"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("already exists"));

    // The original content must be untouched.
    let content = std::fs::read_to_string(dir.join("decon.toml")).unwrap();
    assert_eq!(content, "# pre-existing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_explicit_toml_is_loaded() {
    // A decon.toml with `root` set should drive `crawl` (no --dir) to that repo.
    let dir = fixtures_dir().join("python-lib");
    let cfg_dir = temp_dir("cfg-toml");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let toml_path = cfg_dir.join("decon.toml");
    let toml_text = format!("root = \"{}\"\n", dir.display());
    std::fs::write(&toml_path, toml_text).unwrap();

    decon()
        .current_dir(&cfg_dir)
        .args(["--config"])
        .arg(&toml_path)
        .args(["crawl"])
        .assert()
        .success()
        .stdout(predicate::str::contains("files: 6"))
        .stdout(predicate::str::contains("README.md"));

    let _ = std::fs::remove_dir_all(&cfg_dir);
}

#[test]
fn config_explicit_yaml_is_loaded() {
    let dir = fixtures_dir().join("python-lib");
    let cfg_dir = temp_dir("cfg-yaml");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let yaml_path = cfg_dir.join(".decon.yaml");
    let yaml_text = format!("root: \"{}\"\n", dir.display());
    std::fs::write(&yaml_path, yaml_text).unwrap();

    decon()
        .args(["--config"])
        .arg(&yaml_path)
        .args(["crawl"])
        .assert()
        .success()
        .stdout(predicate::str::contains("files: 6"));

    let _ = std::fs::remove_dir_all(&cfg_dir);
}

#[test]
fn config_discovered_from_cwd_drives_crawl() {
    // With no --config and no --dir, the CLI discovers decon.toml in cwd and
    // uses its `root` to crawl.
    let repo = fixtures_dir().join("python-lib");
    let cwd = temp_dir("cfg-discover");
    std::fs::create_dir_all(&cwd).unwrap();
    let toml_text = format!("root = \"{}\"\n", repo.display());
    std::fs::write(cwd.join("decon.toml"), toml_text).unwrap();

    decon()
        .current_dir(&cwd)
        .args(["crawl"])
        .assert()
        .success()
        .stdout(predicate::str::contains("files: 6"))
        .stdout(predicate::str::contains("README.md"));

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn config_invalid_toml_exits_config() {
    let cfg_dir = temp_dir("cfg-bad");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let bad = cfg_dir.join("decon.toml");
    std::fs::write(&bad, b"this is = not = valid toml =\n").unwrap();

    decon()
        .args(["--config"])
        .arg(&bad)
        .args(["crawl"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: config:"));

    let _ = std::fs::remove_dir_all(&cfg_dir);
}

#[test]
fn config_missing_file_exits_config() {
    decon()
        .args(["--config", "/no/such/decon-missing.toml"])
        .args(["crawl"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: config:"))
        .stderr(predicate::str::contains("read /no/such/decon-missing.toml"));
}

#[test]
fn unknown_subcommand_exits_config() {
    decon()
        .arg("frobnicate")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn crawl_subcommand_help() {
    decon()
        .args(["crawl", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("List relative file inventory"))
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--format"));
}

#[test]
fn dry_run_subcommand_help() {
    decon()
        .args(["dry-run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run plan"))
        .stdout(predicate::str::contains("--apps"));
}

#[test]
fn eval_subcommand_help() {
    decon()
        .args(["eval", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Structural eval"))
        .stdout(predicate::str::contains("--threshold"));
}

#[test]
fn resume_subcommand_help() {
    decon()
        .args(["resume", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("checkpoint"))
        .stdout(predicate::str::contains("--checkpoint"));
}

#[test]
fn init_subcommand_help() {
    decon()
        .args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("starter"))
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--non-interactive"))
        .stdout(predicate::str::contains("--check"));
}

#[test]
fn eval_walks_nested_tutorial_subdirectories() {
    // A tutorial with a nested chapter directory exercises the recursive
    // branch of `walk_md` (subdirectory traversal in main.rs).
    let dir = temp_dir("eval-nested");
    std::fs::create_dir_all(dir.join("chapters")).unwrap();
    std::fs::write(
        dir.join("index.md"),
        b"# Index\n\n## Chapters\n\n- [A](chapters/a.md)\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("chapters/a.md"),
        b"# A\n\nCites `src/main.rs`.\n\n## Evidence\n\nPaths cited above are from the repository inventory.\n",
    )
    .unwrap();

    // The nested chapter was discovered (recursive walk): the index link to
    // `chapters/a.md` resolves and the chapter contributes path citations.
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .args(["--format", "json"])
        .assert()
        .stdout(predicate::str::contains("\"has_index\":true"))
        .stdout(predicate::str::contains("\"links_resolved\":1"))
        .stdout(predicate::str::contains("\"has_path_citations\":true"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn help_lists_identify_subcommand() {
    decon()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("identify"));
}

#[test]
fn identify_single_shot_completes_and_writes_checkpoint() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-identify-ok-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["identify", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .success()
        .stdout(predicate::str::contains("identify: completed"));

    // The checkpoint directory should exist with checkpoint.json.
    assert!(
        ckpt_dir.join("checkpoint.json").is_file(),
        "checkpoint.json should exist"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
fn identify_empty_dir_exits_config() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let empty = std::env::temp_dir().join(format!("decon-cli-identify-empty-{n}"));
    std::fs::create_dir_all(&empty).unwrap();

    decon()
        .args(["identify", "--dir"])
        .arg(&empty)
        .assert()
        .failure()
        .code(2);

    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn identify_missing_dir_exits_config() {
    decon()
        .args(["identify", "--dir", "/no/such/dir/decon-identify-test"])
        .assert()
        .failure()
        .code(2);
}

// ---------------------------------------------------------------------------
// Issue #79: cmd_resume dir-exists check + load_file_config extension
// detection.
// ---------------------------------------------------------------------------

#[test]
fn resume_nonexistent_checkpoint_dir_exits_config_with_specific_message() {
    decon()
        .args(["resume", "--checkpoint", "/no/such/decon-resume-79-missing"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("does not exist"));
}

#[test]
fn resume_checkpoint_path_is_file_not_dir_exits_config() {
    let file = temp_dir("resume-file-not-dir");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"not a directory").unwrap();

    decon()
        .args(["resume", "--checkpoint"])
        .arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("not a directory"));

    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

// ---------------------------------------------------------------------------
// Issue #103: exit codes 3 (budget) and 4 (LLM).
//
// Budget overrun → exit 3; LLM errors (network/timeout/rate-limit/provider/
// parse) → exit 4. Previously both collapsed into exit 1.
// ---------------------------------------------------------------------------

#[test]
fn identify_budget_exceeded_exits_budget_code_3() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-identify-budget-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .env("DECON_MAX_LLM_CALLS", "0")
        .args(["identify", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("budget"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
#[cfg(debug_assertions)]
fn identify_llm_error_exits_llm_code_4() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-identify-llm-err-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .env("DECON_LLM_MOCK_FAIL", "network")
        .args(["identify", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("LLM"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

// ---------------------------------------------------------------------------
// Issue #146: `decon generate` subcommand (full pipeline).
//
// CLI-level tests for argument parsing, help text, and exit codes.
// Pipeline orchestration logic is tested in decon-pipeline::generate.
// ---------------------------------------------------------------------------

#[test]
fn help_lists_generate_subcommand() {
    decon()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("generate"));
}

#[test]
fn generate_subcommand_help_shows_all_flags() {
    decon()
        .args(["generate", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--apps"))
        .stdout(predicate::str::contains("--language"))
        .stdout(predicate::str::contains("--diagram-level"))
        .stdout(predicate::str::contains("--force-setup"))
        .stdout(predicate::str::contains("--no-setup"))
        .stdout(predicate::str::contains("--no-overview"))
        .stdout(predicate::str::contains("--checkpoint-dir"))
        .stdout(predicate::str::contains("--output-dir"))
        .stdout(predicate::str::contains("--max-abstractions"))
        .stdout(predicate::str::contains("--single-shot"))
        .stdout(predicate::str::contains("--each-app"));
}

#[test]
fn generate_without_dir_exits_config() {
    decon().args(["generate"]).assert().failure().code(2);
}

#[test]
fn generate_missing_dir_exits_config() {
    decon()
        .args(["generate", "--dir", "/no/such/decon-generate-test-dir"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn generate_empty_dir_exits_config() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let empty = std::env::temp_dir().join(format!("decon-cli-generate-empty-{n}"));
    std::fs::create_dir_all(&empty).unwrap();

    decon()
        .args(["generate", "--dir"])
        .arg(&empty)
        .assert()
        .failure()
        .code(2);

    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn generate_budget_exceeded_exits_budget_code_3() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-generate-budget-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .env("DECON_MAX_LLM_CALLS", "0")
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("budget"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
fn generate_completes_and_writes_output() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-generate-ok-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-generate-ok-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// ---------------------------------------------------------------------------
// Issue #148: `decon generate --each-app` flag for per-app generation.
// ---------------------------------------------------------------------------

#[test]
fn generate_each_app_help_shows_flag() {
    decon()
        .args(["generate", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--each-app"));
}

#[test]
fn generate_each_app_completes_and_writes_per_app_output() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-each-app-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-each-app-out-{n}"));
    let dir = fixtures_dir().join("umbrella");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--each-app", "--no-setup"])
        .assert()
        .success()
        .stderr(predicate::str::contains("each-app completed"));

    assert!(
        output_dir.join("index.md").is_file(),
        "summary index.md should exist"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
    let alpha_ckpt = std::env::temp_dir().join(format!("decon-cli-each-app-ckpt-{n}-apps-alpha"));
    let beta_ckpt = std::env::temp_dir().join(format!("decon-cli-each-app-ckpt-{n}-apps-beta"));
    let gamma_ckpt = std::env::temp_dir().join(format!("decon-cli-each-app-ckpt-{n}-apps-gamma"));
    let _ = std::fs::remove_dir_all(&alpha_ckpt);
    let _ = std::fs::remove_dir_all(&beta_ckpt);
    let _ = std::fs::remove_dir_all(&gamma_ckpt);
}

// ---------------------------------------------------------------------------
// Issue #147: per-stage subcommands for debugging individual pipeline stages.
//
// Each subcommand runs exactly one stage, reads inputs from checkpoint, and
// writes outputs to checkpoint. These tests cover --help flags, missing --dir
// (exit 2), missing checkpoint (prerequisite error), and a successful run of
// the combine stage with a pre-populated checkpoint.
// ---------------------------------------------------------------------------

#[test]
fn relationships_subcommand_help_shows_flags() {
    decon()
        .args(["relationships", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--checkpoint-dir"))
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn relationships_without_dir_exits_config() {
    decon().args(["relationships"]).assert().failure().code(2);
}

#[test]
fn relationships_missing_checkpoint_exits_config() {
    decon()
        .args(["relationships", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-rel-stage-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("checkpoint"));
}

#[test]
fn order_subcommand_help_shows_flags() {
    decon()
        .args(["order", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--checkpoint-dir"))
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn order_without_dir_exits_config() {
    decon().args(["order"]).assert().failure().code(2);
}

#[test]
fn order_missing_checkpoint_exits_config() {
    decon()
        .args(["order", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-order-stage-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("checkpoint"));
}

#[test]
fn chapters_subcommand_help_shows_flags() {
    decon()
        .args(["chapters", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--checkpoint-dir"))
        .stdout(predicate::str::contains("--output-dir"))
        .stdout(predicate::str::contains("--language"))
        .stdout(predicate::str::contains("--diagram-level"))
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn chapters_without_dir_exits_config() {
    decon().args(["chapters"]).assert().failure().code(2);
}

#[test]
fn chapters_missing_checkpoint_exits_config() {
    decon()
        .args(["chapters", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-chapters-stage-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("checkpoint"));
}

#[test]
fn setup_subcommand_help_shows_flags() {
    decon()
        .args(["setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--checkpoint-dir"))
        .stdout(predicate::str::contains("--force"))
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn setup_without_dir_exits_config() {
    decon().args(["setup"]).assert().failure().code(2);
}

#[test]
fn setup_missing_checkpoint_exits_config() {
    decon()
        .args(["setup", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-setup-stage-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("checkpoint"));
}

#[test]
fn overview_subcommand_help_shows_flags() {
    decon()
        .args(["overview", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--checkpoint-dir"))
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn overview_without_dir_exits_config() {
    decon().args(["overview"]).assert().failure().code(2);
}

#[test]
fn overview_missing_checkpoint_exits_config() {
    decon()
        .args(["overview", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-overview-stage-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("checkpoint"));
}

#[test]
fn combine_subcommand_help_shows_flags() {
    decon()
        .args(["combine", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dir"))
        .stdout(predicate::str::contains("--checkpoint-dir"))
        .stdout(predicate::str::contains("--output-dir"))
        .stdout(predicate::str::contains("--language"))
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn combine_without_dir_exits_config() {
    decon().args(["combine"]).assert().failure().code(2);
}

#[test]
fn combine_missing_checkpoint_exits_config() {
    decon()
        .args(["combine", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-combine-stage-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("checkpoint"));
}

fn seed_full_checkpoint(ckpt_dir: &std::path::Path) {
    use decon_core::{
        Abstraction, AbstractionKind, Chapter, ChapterOrder, ChapterResult, CheckpointV1,
        IdentifyResult, Relationship, RelationshipsResult, RunConfig, StageId, Tier,
    };
    use decon_pipeline::{CheckpointStore, records_from_files};

    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
    meta.mark_stage_complete(StageId::DryRun, "2026-07-24T00:02:00Z");

    let abstractions = vec![
        Abstraction {
            name: "Router".into(),
            description: "Routes requests".into(),
            file_indices: vec![0],
            tier: Tier::S,
            kind: AbstractionKind::new("module"),
            apps: vec!["web".into()],
            entry_files: vec!["src/router.rs".into()],
        },
        Abstraction {
            name: "Store".into(),
            description: "Persistence layer".into(),
            file_indices: vec![1],
            tier: Tier::S,
            kind: AbstractionKind::new("module"),
            apps: vec!["web".into()],
            entry_files: vec!["src/store.rs".into()],
        },
        Abstraction {
            name: "Worker".into(),
            description: "Background jobs".into(),
            file_indices: vec![2],
            tier: Tier::S,
            kind: AbstractionKind::new("module"),
            apps: vec!["api".into()],
            entry_files: vec!["src/worker.rs".into()],
        },
    ];
    let identify = IdentifyResult::new(abstractions);
    meta.abstractions = Some(identify.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Identify, "t3");

    let rels = RelationshipsResult::new(
        "A web framework with routing and persistence.".to_string(),
        vec![Relationship::new(
            0,
            1,
            "calls".to_string(),
            "calls".to_string(),
        )],
    );
    meta.relationships = Some(rels.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Relationships, "t4");

    let order = ChapterOrder::new(vec![0, 1, 2]);
    meta.order = Some(order.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Order, "t5");

    let store = CheckpointStore::new(ckpt_dir);
    let files = records_from_files(&[
        ("src/router.rs", b"fn route() {}" as &[u8]),
        ("src/store.rs", b"fn store() {}" as &[u8]),
        ("src/worker.rs", b"fn work() {}" as &[u8]),
    ]);
    store.save(meta.clone(), &files).unwrap();

    let chapters = ChapterResult::new(vec![
        Chapter::new(0, 1, "Router", "# Router\n", Tier::S, "module", "f0"),
        Chapter::new(1, 2, "Store", "# Store\n", Tier::S, "module", "f1"),
        Chapter::new(2, 3, "Worker", "# Worker\n", Tier::S, "module", "f2"),
    ]);
    let entries = store.write_chapters(&store.dir, &chapters).unwrap();
    let (mut meta2, files2) = store.load().unwrap();
    meta2.mark_stage_complete(StageId::Chapters, "t6");
    store
        .record_stage_outputs(&mut meta2, StageId::Chapters, entries)
        .unwrap();
    store.save(meta2.clone(), &files2).unwrap();
}

#[test]
fn combine_with_prepopulated_checkpoint_runs_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-combine-stage-ok-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-combine-stage-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    seed_full_checkpoint(&ckpt_dir);

    decon()
        .args(["combine", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("combine: completed"));

    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn combine_with_incomplete_checkpoint_errors_about_prerequisites() {
    use decon_core::{CheckpointV1, RunConfig, StageId};
    use decon_pipeline::{CheckpointStore, records_from_files};
    use std::time::{SystemTime, UNIX_EPOCH};

    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-combine-stage-incomplete-{n}"));
    let output_dir =
        std::env::temp_dir().join(format!("decon-cli-combine-stage-incomplete-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "t1");
    meta.mark_stage_complete(StageId::DryRun, "t2");
    meta.mark_stage_complete(StageId::Identify, "t3");
    let files = records_from_files(&[("a.txt", b"hi" as &[u8])]);
    CheckpointStore::new(&ckpt_dir).save(meta, &files).unwrap();

    decon()
        .args(["combine", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("chapters"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// ---------------------------------------------------------------------------
// Issue #183: Configurable concurrency limits + better error UX.
//
// --concurrency, --max-llm-calls, --verbose, --quiet flags on `decon generate`.
// ---------------------------------------------------------------------------

#[test]
fn generate_help_shows_new_flags() {
    decon()
        .args(["generate", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--max-llm-calls"))
        .stdout(predicate::str::contains("--verbose"))
        .stdout(predicate::str::contains("--quiet"));
}

#[test]
fn generate_verbose_and_quiet_are_mutually_exclusive() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--verbose", "--quiet"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("mutually exclusive"));
}

#[test]
fn generate_concurrency_zero_exits_config() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--concurrency", "0"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("positive integer"));
}

#[test]
fn generate_max_llm_calls_zero_exits_config() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--max-llm-calls", "0"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("positive integer"));
}

#[test]
fn generate_budget_flag_respected_exits_budget_code_3() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-budget-flag-{n}"));
    let dir = fixtures_dir().join("python-lib");

    // --max-llm-calls 1 should exhaust the budget quickly (identify alone
    // needs at least 1 call, then relationships/order/chapters need more).
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot", "--max-llm-calls", "1"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("budget"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
fn generate_concurrency_flag_completes_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-concurrency-ok-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-concurrency-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--concurrency", "2"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_verbose_output_contains_detail_lines() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-verbose-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-verbose-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--verbose"])
        .assert()
        .success()
        // Verbose mode prints "verbose:" prefixed lines to stderr.
        .stderr(predicate::str::contains("verbose: concurrency="))
        .stderr(predicate::str::contains("verbose: llm-calls:"))
        .stderr(predicate::str::contains("verbose: checkpoint:"))
        // Stage timing lines mention a stage name and elapsed time.
        .stderr(predicate::str::contains("verbose: stage "));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_quiet_mode_suppresses_progress_output() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-quiet-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-quiet-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    let result = decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--quiet"])
        .assert()
        .success();

    let stderr = &result.get_output().stderr;
    let stderr_str = String::from_utf8_lossy(stderr);

    // In quiet mode, no progress messages should appear.
    assert!(
        !stderr_str.contains("generate: completed"),
        "quiet mode should suppress 'generate: completed', got: {stderr_str}"
    );
    assert!(
        !stderr_str.contains("warning: generate:"),
        "quiet mode should suppress warnings, got: {stderr_str}"
    );

    // But the output file should still be created.
    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir even in quiet mode"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_error_includes_actionable_hint() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-hint-{n}"));
    let dir = fixtures_dir().join("python-lib");

    // Budget exhaustion should produce a "hint:" line with an actionable
    // suggestion mentioning --max-llm-calls or resume.
    decon()
        .env("DECON_MAX_LLM_CALLS", "0")
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("hint:"))
        .stderr(predicate::str::contains("--max-llm-calls"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

// ---------------------------------------------------------------------------
// Issue #186: M5-TST-2 — CLI error path tests + coverage improvement.
//
// Comprehensive tests for all exit codes (0–5), malformed user input, error
// message content, and flag coverage to push main.rs coverage from 64% to
// ≥80%.
// ---------------------------------------------------------------------------

// --- Exit code 5: partial/checkpoint (cancellation) ---

#[test]
#[cfg(debug_assertions)]
fn identify_cancelled_exits_partial_code_5() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-identify-cancel-{n}"));
    let dir = fixtures_dir().join("python-lib");

    // DECON_MOCK_CANCEL causes the cancel token to fire immediately, so the
    // identify stage returns `Cancelled` → exit 5.
    decon()
        .env("DECON_MOCK_CANCEL", "1")
        .args(["identify", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(5)
        .stderr(predicate::str::contains("cancelled"))
        .stderr(predicate::str::contains("partial checkpoint"))
        .stderr(predicate::str::contains("resume to continue"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
#[cfg(debug_assertions)]
fn generate_cancelled_exits_partial_code_5() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-cancel-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-cancel-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .env("DECON_MOCK_CANCEL", "1")
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(5)
        .stderr(predicate::str::contains("cancelled"))
        .stderr(predicate::str::contains("partial checkpoint"))
        .stderr(predicate::str::contains("resume to continue"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// --- Exit code 4: LLM error on `generate` ---

#[test]
#[cfg(debug_assertions)]
fn generate_llm_error_exits_llm_code_4() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-llm-err-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .env("DECON_LLM_MOCK_FAIL", "network")
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("error: generate failed:"))
        .stderr(predicate::str::contains("LLM"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
#[cfg(debug_assertions)]
fn generate_llm_error_includes_hint_about_api_key() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-llm-hint-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .env("DECON_LLM_MOCK_FAIL", "timeout")
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("hint:"))
        .stderr(predicate::str::contains("DECON_LLM_API_KEY"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

// --- Exit code 0: success with various flag combinations ---

#[test]
fn generate_review_chapters_completes_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-review-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-review-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--review-chapters", "--no-setup"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_no_setup_no_overview_completes_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-nosetup-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-nosetup-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--no-setup", "--no-overview"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_force_setup_completes_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-force-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-force-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--force-setup"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_verbose_with_concurrency_completes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-verb-conc-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-verb-conc-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--verbose", "--concurrency", "1"])
        .assert()
        .success()
        .stderr(predicate::str::contains("verbose: concurrency=1"))
        .stderr(predicate::str::contains("generate: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_max_llm_calls_flag_completes_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-maxcalls-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-maxcalls-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    // Provide a generous budget so the pipeline completes.
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--max-llm-calls", "100"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// --- Malformed input: invalid types, bad flags, missing args ---

#[test]
fn eval_invalid_threshold_type_exits_config() {
    // clap rejects non-integer values for --threshold with exit code 2.
    let dir = fixtures_dir().join("tutorials/good-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .args(["--threshold", "not-a-number"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn generate_invalid_diagram_level_exits_config() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--diagram-level", "bogus"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid diagram level"))
        .stderr(predicate::str::contains("minimal, standard, or rich"));
}

#[test]
fn chapters_invalid_diagram_level_exits_config() {
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["chapters", "--dir"])
        .arg(&dir)
        .args(["--diagram-level", "bogus"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid diagram level"))
        .stderr(predicate::str::contains("minimal, standard, or rich"));
}

#[test]
fn generate_invalid_concurrency_type_exits_config() {
    // clap rejects non-integer values for --concurrency.
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--concurrency", "abc"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn generate_invalid_max_llm_calls_type_exits_config() {
    // clap rejects non-integer values for --max-llm-calls.
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--max-llm-calls", "abc"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn crawl_invalid_format_exits_config() {
    // clap rejects non-enum values for --format.
    let dir = fixtures_dir().join("python-lib");
    decon()
        .args(["crawl", "--dir"])
        .arg(&dir)
        .args(["--format", "xml"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn eval_missing_out_arg_exits_config() {
    // eval without --out defaults to "output" which likely doesn't exist.
    decon()
        .args(["eval"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("eval failed to load tutorial"));
}

#[test]
fn generate_missing_dir_arg_exits_config() {
    // generate requires --dir; clap rejects missing required arg with code 2.
    decon().args(["generate"]).assert().failure().code(2);
}

#[test]
fn config_path_with_no_filename_exits_config() {
    // A path like "/" has no file name component; the loader must error.
    decon()
        .args(["--config", "/"])
        .args(["crawl"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("config:"))
        .stderr(predicate::str::contains("file name"));
}

// --- Error message content assertions ---

#[test]
fn crawl_error_message_contains_crawl_failed() {
    decon()
        .args(["crawl", "--dir", "/no/such/decon-crawl-msg-test"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: crawl failed:"));
}

#[test]
fn dry_run_error_message_contains_dry_run_failed() {
    decon()
        .args(["dry-run", "--dir", "/no/such/decon-dryrun-msg-test"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: dry-run failed:"));
}

#[test]
fn eval_error_message_contains_eval_failed() {
    decon()
        .args(["eval", "--out", "/no/such/decon-eval-msg-test"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "error: eval failed to load tutorial:",
        ));
}

#[test]
fn identify_error_message_contains_identify_failed() {
    decon()
        .args(["identify", "--dir", "/no/such/decon-identify-msg-test"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: identify: crawl failed:"));
}

#[test]
fn generate_empty_dir_error_message_contains_no_files() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let empty = std::env::temp_dir().join(format!("decon-cli-gen-empty-msg-{n}"));
    std::fs::create_dir_all(&empty).unwrap();

    decon()
        .args(["generate", "--dir"])
        .arg(&empty)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no files found"));

    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn generate_budget_error_message_contains_budget_and_hint() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-budget-msg-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .env("DECON_MAX_LLM_CALLS", "0")
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("error: generate failed:"))
        .stderr(predicate::str::contains("budget"))
        .stderr(predicate::str::contains("hint:"))
        .stderr(predicate::str::contains("--max-llm-calls"))
        .stderr(predicate::str::contains("resume"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

// --- Verbose and quiet mode content assertions ---

#[test]
fn generate_verbose_shows_concurrency_and_max_llm_calls() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-verb-detail-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-verb-detail-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--verbose", "--concurrency", "3"])
        .assert()
        .success()
        // The verbose line prints "concurrency=N max-llm-calls=M" together.
        .stderr(predicate::str::contains("verbose: concurrency=3"))
        .stderr(predicate::str::contains("max-llm-calls="))
        .stderr(predicate::str::contains("verbose: llm-calls:"))
        .stderr(predicate::str::contains("verbose: stage "))
        .stderr(predicate::str::contains("verbose: checkpoint:"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_quiet_suppresses_all_progress() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-quiet-suppress-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-quiet-suppress-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    let result = decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--quiet"])
        .assert()
        .success();

    let stderr = &result.get_output().stderr;
    let stderr_str = String::from_utf8_lossy(stderr);

    // In quiet mode, no progress or verbose messages should appear.
    assert!(
        !stderr_str.contains("generate: completed"),
        "quiet mode should suppress 'generate: completed', got: {stderr_str}"
    );
    assert!(
        !stderr_str.contains("verbose:"),
        "quiet mode should suppress verbose messages, got: {stderr_str}"
    );
    assert!(
        !stderr_str.contains("warning:"),
        "quiet mode should suppress warnings, got: {stderr_str}"
    );

    // But the output file should still be created.
    assert!(
        output_dir.join("index.md").is_file(),
        "index.md should exist in output dir even in quiet mode"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// --- Per-stage subcommand error message content ---

#[test]
fn relationships_missing_checkpoint_message_contains_checkpoint() {
    decon()
        .args(["relationships", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-rel-msg-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "does not exist or is not a directory",
        ));
}

#[test]
fn order_missing_checkpoint_message_contains_checkpoint() {
    decon()
        .args(["order", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-order-msg-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "does not exist or is not a directory",
        ));
}

#[test]
fn chapters_missing_checkpoint_message_contains_checkpoint() {
    decon()
        .args(["chapters", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-chapters-msg-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "does not exist or is not a directory",
        ));
}

#[test]
fn setup_missing_checkpoint_message_contains_checkpoint() {
    decon()
        .args(["setup", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-setup-msg-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "does not exist or is not a directory",
        ));
}

#[test]
fn overview_missing_checkpoint_message_contains_checkpoint() {
    decon()
        .args(["overview", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-overview-msg-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "does not exist or is not a directory",
        ));
}

#[test]
fn combine_missing_checkpoint_message_contains_checkpoint() {
    decon()
        .args(["combine", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .args(["--checkpoint-dir", "/no/such/decon-combine-msg-ckpt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "does not exist or is not a directory",
        ));
}

// --- Config error message content ---

#[test]
fn config_invalid_yaml_exits_config() {
    let cfg_dir = temp_dir("cfg-bad-yaml");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let bad = cfg_dir.join(".decon.yaml");
    std::fs::write(&bad, b"root: [invalid: yaml: content\n").unwrap();

    decon()
        .args(["--config"])
        .arg(&bad)
        .args(["crawl"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: config:"));

    let _ = std::fs::remove_dir_all(&cfg_dir);
}

#[test]
fn config_unparseable_content_exits_config() {
    let cfg_dir = temp_dir("cfg-unparseable");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let bad = cfg_dir.join("decon.cfg");
    std::fs::write(&bad, b"\x00\x01\x02 binary garbage \x03\x04\n").unwrap();

    decon()
        .args(["--config"])
        .arg(&bad)
        .args(["crawl"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: config:"));

    let _ = std::fs::remove_dir_all(&cfg_dir);
}

// --- Exit code 1: generic failure (structural eval fail) ---

#[test]
fn eval_broken_mini_exits_fail_code_1() {
    let dir = fixtures_dir().join("tutorials/broken-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("passed=false"));
}

#[test]
fn eval_broken_mini_text_format_contains_reasons() {
    let dir = fixtures_dir().join("tutorials/broken-mini");
    decon()
        .args(["eval", "--out"])
        .arg(&dir)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("score="))
        .stdout(predicate::str::contains("passed=false"))
        .stdout(predicate::str::contains("- "));
}

// --- Init error paths ---

#[test]
fn init_nonexistent_parent_dir_exits_config() {
    decon()
        .args(["init", "--dir", "/no/such/parent/decon-init-test/sub"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error: create"));
}

// --- Generate with --each-app verbose mode ---

#[test]
fn generate_each_app_verbose_completes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-each-app-verb-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-each-app-verb-out-{n}"));
    let dir = fixtures_dir().join("umbrella");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--each-app", "--no-setup", "--verbose"])
        .assert()
        .success()
        .stderr(predicate::str::contains("verbose: concurrency="))
        .stderr(predicate::str::contains("each-app completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
    let alpha_ckpt =
        std::env::temp_dir().join(format!("decon-cli-each-app-verb-ckpt-{n}-apps-alpha"));
    let beta_ckpt =
        std::env::temp_dir().join(format!("decon-cli-each-app-verb-ckpt-{n}-apps-beta"));
    let gamma_ckpt =
        std::env::temp_dir().join(format!("decon-cli-each-app-verb-ckpt-{n}-apps-gamma"));
    let _ = std::fs::remove_dir_all(&alpha_ckpt);
    let _ = std::fs::remove_dir_all(&beta_ckpt);
    let _ = std::fs::remove_dir_all(&gamma_ckpt);
}

// ---------------------------------------------------------------------------
// Per-stage subcommand success paths (improves main.rs coverage).
//
// These exercise the cmd_relationships, cmd_order, cmd_chapters, cmd_setup,
// and cmd_overview functions end-to-end with pre-populated checkpoints.
// ---------------------------------------------------------------------------

/// Seed a checkpoint with Fetch + DryRun + Identify complete.
fn seed_identify_checkpoint(ckpt_dir: &std::path::Path) {
    use decon_core::{
        Abstraction, AbstractionKind, CheckpointV1, IdentifyResult, RunConfig, StageId, Tier,
    };
    use decon_pipeline::{CheckpointStore, records_from_files};

    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
    meta.mark_stage_complete(StageId::DryRun, "2026-07-24T00:02:00Z");

    let abstractions = vec![Abstraction {
        name: "Router".into(),
        description: "Routes requests".into(),
        file_indices: vec![0],
        tier: Tier::S,
        kind: AbstractionKind::new("module"),
        apps: vec!["web".into()],
        entry_files: vec!["src/router.rs".into()],
    }];
    let identify = IdentifyResult::new(abstractions);
    meta.abstractions = Some(identify.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Identify, "t3");

    let files = records_from_files(&[("src/router.rs", b"fn route() {}" as &[u8])]);
    CheckpointStore::new(ckpt_dir).save(meta, &files).unwrap();
}

/// Seed a checkpoint with Fetch + DryRun + Identify + Relationships complete.
fn seed_relationships_checkpoint(ckpt_dir: &std::path::Path) {
    use decon_core::{
        Abstraction, AbstractionKind, CheckpointV1, IdentifyResult, Relationship,
        RelationshipsResult, RunConfig, StageId, Tier,
    };
    use decon_pipeline::{CheckpointStore, records_from_files};

    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "t1");
    meta.mark_stage_complete(StageId::DryRun, "t2");

    let abstractions = vec![Abstraction {
        name: "Router".into(),
        description: "Routes requests".into(),
        file_indices: vec![0],
        tier: Tier::S,
        kind: AbstractionKind::new("module"),
        apps: vec!["web".into()],
        entry_files: vec!["src/router.rs".into()],
    }];
    let identify = IdentifyResult::new(abstractions);
    meta.abstractions = Some(identify.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Identify, "t3");

    let rels = RelationshipsResult::new(
        "A web framework.".to_string(),
        vec![Relationship::new(
            0,
            0,
            "self".to_string(),
            "self".to_string(),
        )],
    );
    meta.relationships = Some(rels.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Relationships, "t4");

    let files = records_from_files(&[("src/router.rs", b"fn route() {}" as &[u8])]);
    CheckpointStore::new(ckpt_dir).save(meta, &files).unwrap();
}

/// Seed a checkpoint with Fetch + DryRun + Identify + Relationships + Order.
fn seed_order_checkpoint(ckpt_dir: &std::path::Path) {
    use decon_core::{
        Abstraction, AbstractionKind, ChapterOrder, CheckpointV1, IdentifyResult, Relationship,
        RelationshipsResult, RunConfig, StageId, Tier,
    };
    use decon_pipeline::{CheckpointStore, records_from_files};

    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "t1");
    meta.mark_stage_complete(StageId::DryRun, "t2");

    let abstractions = vec![Abstraction {
        name: "Router".into(),
        description: "Routes requests".into(),
        file_indices: vec![0],
        tier: Tier::S,
        kind: AbstractionKind::new("module"),
        apps: vec!["web".into()],
        entry_files: vec!["src/router.rs".into()],
    }];
    let identify = IdentifyResult::new(abstractions);
    meta.abstractions = Some(identify.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Identify, "t3");

    let rels = RelationshipsResult::new(
        "A web framework.".to_string(),
        vec![Relationship::new(
            0,
            0,
            "self".to_string(),
            "self".to_string(),
        )],
    );
    meta.relationships = Some(rels.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Relationships, "t4");

    let order = ChapterOrder::new(vec![0]);
    meta.order = Some(order.to_checkpoint_value().unwrap());
    meta.mark_stage_complete(StageId::Order, "t5");

    let files = records_from_files(&[("src/router.rs", b"fn route() {}" as &[u8])]);
    CheckpointStore::new(ckpt_dir).save(meta, &files).unwrap();
}

#[test]
fn relationships_stage_runs_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-rel-stage-ok-{n}"));
    let dir = fixtures_dir().join("python-lib");

    seed_identify_checkpoint(&ckpt_dir);

    decon()
        .args(["relationships", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("relationships: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
fn order_stage_runs_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-order-stage-ok-{n}"));
    let dir = fixtures_dir().join("python-lib");

    seed_relationships_checkpoint(&ckpt_dir);

    decon()
        .args(["order", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("order: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
fn chapters_stage_runs_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-chapters-stage-ok-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-chapters-stage-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    seed_order_checkpoint(&ckpt_dir);

    decon()
        .args(["chapters", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("chapters: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn setup_stage_runs_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-setup-stage-ok-{n}"));
    let dir = fixtures_dir().join("python-lib");

    seed_identify_checkpoint(&ckpt_dir);

    decon()
        .args(["setup", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--force"])
        .assert()
        .success()
        .stderr(predicate::str::contains("setup: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
fn overview_stage_runs_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-overview-stage-ok-{n}"));
    let dir = fixtures_dir().join("umbrella");

    seed_relationships_checkpoint(&ckpt_dir);

    decon()
        .args(["overview", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("overview: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

// --- Per-stage subcommand error: missing identify in checkpoint ---

#[test]
fn relationships_stage_without_identify_exits_config() {
    use decon_core::{CheckpointV1, RunConfig, StageId};
    use decon_pipeline::{CheckpointStore, records_from_files};
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-rel-no-identify-{n}"));
    let dir = fixtures_dir().join("python-lib");

    // Create a checkpoint with only Fetch + DryRun (no Identify).
    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "t1");
    meta.mark_stage_complete(StageId::DryRun, "t2");
    let files = records_from_files(&[("a.txt", b"hi" as &[u8])]);
    CheckpointStore::new(&ckpt_dir).save(meta, &files).unwrap();

    decon()
        .args(["relationships", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .assert()
        .failure()
        .code(2);

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

#[test]
fn chapters_stage_without_identify_exits_config() {
    use decon_core::{CheckpointV1, RunConfig, StageId};
    use decon_pipeline::{CheckpointStore, records_from_files};
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-chap-no-identify-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-chap-no-identify-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    let cfg = RunConfig::default();
    let mut meta = CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        ".",
        "2026-07-24T00:00:00Z",
    )
    .unwrap();
    meta.mark_stage_complete(StageId::Fetch, "t1");
    meta.mark_stage_complete(StageId::DryRun, "t2");
    let files = records_from_files(&[("a.txt", b"hi" as &[u8])]);
    CheckpointStore::new(&ckpt_dir).save(meta, &files).unwrap();

    decon()
        .args(["chapters", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("identify result not found"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// --- Generate with --language flag ---

#[test]
fn generate_with_language_flag_completes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-lang-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-lang-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--language", "es", "--no-setup"])
        .assert()
        .success()
        .stderr(predicate::str::contains("locale=es"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// --- Generate with --diagram-level flag ---

#[test]
fn generate_with_rich_diagram_level_completes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-rich-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-rich-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--diagram-level", "rich", "--no-setup"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn generate_with_minimal_diagram_level_completes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-min-diagram-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-min-diagram-out-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--diagram-level", "minimal", "--no-setup"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// --- Generate with --apps flag ---

#[test]
fn generate_with_apps_flag_completes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-gen-apps-ckpt-{n}"));
    let output_dir = std::env::temp_dir().join(format!("decon-cli-gen-apps-out-{n}"));
    let dir = fixtures_dir().join("umbrella");

    decon()
        .args(["generate", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--output-dir"])
        .arg(&output_dir)
        .args(["--single-shot", "--apps", "apps/alpha", "--no-setup"])
        .assert()
        .success()
        .stderr(predicate::str::contains("generate: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
    let _ = std::fs::remove_dir_all(&output_dir);
}

// --- Identify with map+reduce (non-single-shot) mode ---

#[test]
fn identify_map_reduce_completes_successfully() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-identify-mapreduce-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["identify", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("identify: completed"));

    assert!(
        ckpt_dir.join("checkpoint.json").is_file(),
        "checkpoint.json should exist"
    );

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

// --- Identify with --max-abstractions flag ---

#[test]
fn identify_with_max_abstractions_completes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ckpt_dir = std::env::temp_dir().join(format!("decon-cli-identify-maxabs-{n}"));
    let dir = fixtures_dir().join("python-lib");

    decon()
        .args(["identify", "--dir"])
        .arg(&dir)
        .args(["--checkpoint-dir"])
        .arg(&ckpt_dir)
        .args(["--single-shot", "--max-abstractions", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("identify: completed"));

    let _ = std::fs::remove_dir_all(&ckpt_dir);
}

// ---------------------------------------------------------------------------
// Issue #185: decon init wizard, --non-interactive, --check
// ---------------------------------------------------------------------------

#[test]
fn init_non_interactive_writes_valid_config() {
    let dir = temp_dir("init-non-interactive");
    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--non-interactive"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote"));

    assert!(dir.join("decon.toml").is_file());
    let content = std::fs::read_to_string(dir.join("decon.toml")).unwrap();
    // Should contain all M5 options as comments.
    for option in &[
        "language",
        "diagram_level",
        "max_abstractions",
        "concurrency",
        "max_llm_calls",
        "cache_dir",
        "cache_size_limit_mb",
        "allowed_hosts",
    ] {
        assert!(content.contains(option), "config should mention {option}");
    }
    // Should contain the API key warning.
    assert!(
        content.contains("DECON_LLM_API_KEY") || content.contains("API keys"),
        "config should warn about API keys"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_non_interactive_config_is_loadable() {
    let dir = temp_dir("init-loadable");
    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--non-interactive"])
        .assert()
        .success();

    // The generated config should be loadable by the CLI (via --config).
    decon()
        .args(["--config"])
        .arg(dir.join("decon.toml"))
        .args(["crawl", "--dir"])
        .arg(fixtures_dir().join("python-lib"))
        .assert()
        .success()
        .stdout(predicate::str::contains("files: 6"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_check_valid_config_exits_zero() {
    let dir = temp_dir("init-check-valid");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("decon.toml"),
        b"language = \"en\"\nconcurrency = 4\n",
    )
    .unwrap();

    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("OK"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_check_invalid_concurrency_exits_two() {
    let dir = temp_dir("init-check-concurrency");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("decon.toml"), b"concurrency = 0\n").unwrap();

    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--check"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicate::str::contains("concurrency"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_check_invalid_diagram_level_exits_two() {
    let dir = temp_dir("init-check-diagram");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("decon.toml"), b"diagram_level = \"ultra\"\n").unwrap();

    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--check"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicate::str::contains("diagram_level"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_check_secret_field_exits_two() {
    let dir = temp_dir("init-check-secret");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("decon.toml"), b"api_key = \"xxx\"\n").unwrap();

    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--check"])
        .assert()
        .failure()
        .code(2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_check_missing_file_exits_two() {
    let dir = temp_dir("init-check-missing");
    std::fs::create_dir_all(&dir).unwrap();

    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--check"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("does not exist"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_check_invalid_toml_exits_two() {
    let dir = temp_dir("init-check-bad-toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("decon.toml"), b"this is = not = valid =\n").unwrap();

    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .args(["--check"])
        .assert()
        .failure()
        .code(2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_help_shows_new_flags() {
    decon()
        .args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--non-interactive"))
        .stdout(predicate::str::contains("--check"));
}

#[test]
fn init_wizard_with_piped_input_writes_config() {
    let dir = temp_dir("init-wizard-piped");
    // Pipe input to the wizard: language, diagram level, max abstractions,
    // concurrency, cache dir (blank), cache size.
    let input = "es\nrich\n15\n8\n\n200\n";
    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .write_stdin(input)
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote"));

    assert!(dir.join("decon.toml").is_file());
    let content = std::fs::read_to_string(dir.join("decon.toml")).unwrap();
    // The wizard answers should be reflected in the config.
    assert!(content.contains("language = \"es\""));
    assert!(content.contains("diagram_level = \"rich\""));
    assert!(content.contains("max_abstractions = 15"));
    assert!(content.contains("concurrency = 8"));
    assert!(content.contains("cache_size_limit_mb = 200"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_wizard_with_eof_uses_defaults() {
    let dir = temp_dir("init-wizard-eof");
    // Empty stdin (immediate EOF) — wizard should fall back to defaults.
    decon()
        .args(["init", "--dir"])
        .arg(&dir)
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote"));

    assert!(dir.join("decon.toml").is_file());
    let content = std::fs::read_to_string(dir.join("decon.toml")).unwrap();
    // All lines should be comments (defaults).
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            assert!(
                trimmed.starts_with('#'),
                "expected all lines to be comments with default answers, got: {trimmed}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Issue #188: man page generation ---

#[test]
fn manpage_to_stdout_is_valid_troff() {
    decon()
        .arg("manpage")
        .assert()
        .success()
        .stdout(predicate::str::contains(".TH "));
}

#[test]
fn manpage_contains_all_subcommands() {
    let out = decon().arg("manpage").assert().success();
    let stdout = &out.get_output().stdout;
    let text = String::from_utf8(stdout.to_vec()).expect("valid UTF-8");
    for name in &[
        "init",
        "crawl",
        "dry-run",
        "eval",
        "resume",
        "identify",
        "generate",
        "relationships",
        "order",
        "chapters",
        "setup",
        "overview",
        "combine",
        "manpage",
    ] {
        assert!(
            text.contains(name),
            "man page should mention subcommand '{name}'"
        );
    }
}

#[test]
fn manpage_contains_key_sections() {
    let out = decon().arg("manpage").assert().success();
    let stdout = &out.get_output().stdout;
    let text = String::from_utf8(stdout.to_vec()).expect("valid UTF-8");
    for section in &["SYNOPSIS", "DESCRIPTION", "OPTIONS", "SUBCOMMANDS"] {
        assert!(
            text.contains(&format!(".SH {section}")),
            "man page should have a {section} section"
        );
    }
    for section in &[
        "EXAMPLES",
        "ENVIRONMENT",
        "FILES",
        "EXIT STATUS",
        "SEE ALSO",
    ] {
        assert!(
            text.contains(&format!(".SH \"{section}\"")),
            "man page should have a {section} section"
        );
    }
}

#[test]
fn manpage_output_flag_writes_file() {
    let dir = temp_dir("manpage-output-flag");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("decon.1");
    decon()
        .args(["manpage", "--output"])
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote"));

    assert!(path.is_file(), "man page file should exist");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains(".TH "),
        "written man page should be valid troff"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn manpage_does_not_require_config() {
    // The manpage subcommand should work even when a broken decon.toml is
    // present in the cwd — it must not load config.
    let dir = temp_dir("manpage-broken-config");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("decon.toml"), b"this is not valid toml = = =\n").unwrap();

    let mut cmd = decon();
    cmd.current_dir(&dir);
    cmd.arg("manpage")
        .assert()
        .success()
        .stdout(predicate::str::contains(".TH "));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// `decon completions` — shell completion script generation (M5-DIST-1)
// ---------------------------------------------------------------------------

/// Subcommand names that must appear in every generated completion script.
const COMPLETION_SUBCOMMANDS: &[&str] = &[
    "crawl",
    "dry-run",
    "eval",
    "resume",
    "init",
    "identify",
    "generate",
    "relationships",
    "order",
    "chapters",
    "setup",
    "overview",
    "combine",
    "completions",
];

#[test]
fn completions_bash_produces_nonempty_output_with_subcommands() {
    let output = decon()
        .args(["completions", "--shell", "bash"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    refute_empty_and_check_subcommands(&output, "bash");
}

#[test]
fn completions_zsh_produces_nonempty_output_with_subcommands() {
    let output = decon()
        .args(["completions", "--shell", "zsh"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    refute_empty_and_check_subcommands(&output, "zsh");
}

#[test]
fn completions_fish_produces_nonempty_output_with_subcommands() {
    let output = decon()
        .args(["completions", "--shell", "fish"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    refute_empty_and_check_subcommands(&output, "fish");
}

#[test]
fn completions_powershell_produces_nonempty_output_with_subcommands() {
    let output = decon()
        .args(["completions", "--shell", "powershell"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    refute_empty_and_check_subcommands(&output, "powershell");
}

#[test]
fn completions_invalid_shell_exits_nonzero() {
    decon()
        .args(["completions", "--shell", "tcsh"])
        .assert()
        .failure();
}

#[test]
fn completions_missing_shell_flag_exits_nonzero() {
    decon().args(["completions"]).assert().failure();
}

#[test]
fn completions_output_flag_writes_file() {
    let dir = temp_dir("completions-output");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("decon.bash");
    decon()
        .args(["completions", "--shell", "bash", "--output"])
        .arg(&path)
        .assert()
        .success();
    assert!(path.is_file(), "completion file should exist");
    let content = std::fs::read_to_string(&path).expect("read completion file");
    assert!(!content.is_empty(), "completion file should not be empty");
    assert!(
        content.contains("_decon"),
        "bash completion should define _decon function"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn completions_works_without_config_file() {
    // The completions subcommand must not require a decon.toml or any run-time
    // argument. Run it from a temp dir with no config to prove this.
    let dir = temp_dir("completions-no-config");
    std::fs::create_dir_all(&dir).unwrap();
    decon()
        .current_dir(&dir)
        .args(["completions", "--shell", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_decon"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Assert the completion output is non-empty and contains every expected
/// subcommand name.
fn refute_empty_and_check_subcommands(output: &[u8], shell: &str) {
    assert!(
        !output.is_empty(),
        "{shell} completion output must not be empty"
    );
    let text = String::from_utf8_lossy(output);
    for sub in COMPLETION_SUBCOMMANDS {
        assert!(
            text.contains(sub),
            "{shell} completion should mention subcommand '{sub}'"
        );
    }
}
