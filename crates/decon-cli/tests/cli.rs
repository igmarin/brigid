//! Smoke tests for the `decon` binary's argument parsing and M1 subcommands.
//!
//! These exercise process-boundary behavior (exit code, `--help`, subcommand
//! contracts). Pipeline logic is unit-tested in library crates.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn decon() -> Command {
    Command::cargo_bin("decon").expect("decon binary should build")
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
        .stdout(predicate::str::contains("--dir"));
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
        .stdout(predicate::str::contains("--single-shot"));
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
