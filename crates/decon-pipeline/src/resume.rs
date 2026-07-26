//! Resume helpers: stage skip, next stage, partial regenerate.
//!
//! Pure decision logic over [`CheckpointV1`] + current run identity. Does not
//! perform I/O; pair with [`crate::checkpoint_store::CheckpointStore`] to load
//! checkpoints from disk.
//!
//! # Resume matrix (tested)
//!
//! | Situation | Behavior |
//! |-----------|----------|
//! | Empty / no completed stages | Run all stages from `Fetch` |
//! | Prefix of pipeline completed | Skip those; run first incomplete |
//! | All pipeline stages complete | Nothing left to run |
//! | Config hash mismatch | Treat as unusable for skip (caller re-crawls) |
//! | Source revision mismatch | Same as config mismatch |
//! | Partial regenerate from stage S | Clear S and all later stages |

use decon_core::config::RunConfig;
use decon_core::{CheckpointV1, StageId, config_hash};

/// Why a checkpoint cannot be used for stage-skipping (caller should re-crawl).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeIdentityMismatch {
    /// Current unredacted config hash differs from checkpoint.
    ConfigHash {
        /// Hash recorded in the checkpoint.
        expected: String,
        /// Hash of the current run config.
        actual: String,
    },
    /// Source revision (git SHA / URL) differs.
    SourceRevision {
        /// Value in the checkpoint.
        expected: String,
        /// Current source identity.
        actual: String,
    },
}

/// Check whether checkpoint identity matches the current run.
///
/// # Errors
///
/// Returns [`ResumeIdentityMismatch`] when config hash or source revision differ.
///
/// # Panics
///
/// Never panics.
pub fn check_identity(
    checkpoint: &CheckpointV1,
    current_config: &RunConfig,
    current_source_revision: &str,
) -> Result<(), ResumeIdentityMismatch> {
    let actual = config_hash(current_config).map_err(|_| ResumeIdentityMismatch::ConfigHash {
        expected: checkpoint.config_hash.clone(),
        actual: String::from("<hash-error>"),
    })?;
    if actual != checkpoint.config_hash {
        return Err(ResumeIdentityMismatch::ConfigHash {
            expected: checkpoint.config_hash.clone(),
            actual,
        });
    }
    if current_source_revision != checkpoint.metadata.source_revision {
        return Err(ResumeIdentityMismatch::SourceRevision {
            expected: checkpoint.metadata.source_revision.clone(),
            actual: current_source_revision.to_owned(),
        });
    }
    Ok(())
}

/// Whether `stage` still needs to run given completed stages.
///
/// Does **not** check identity; call [`check_identity`] first when resuming
/// from disk.
#[must_use]
pub fn should_run(stage: StageId, checkpoint: &CheckpointV1) -> bool {
    !checkpoint.is_stage_complete(stage)
}

/// First incomplete stage in pipeline order, if any.
#[must_use]
pub fn next_stage(checkpoint: &CheckpointV1) -> Option<StageId> {
    StageId::pipeline_order()
        .iter()
        .copied()
        .find(|s| should_run(*s, checkpoint))
}

/// Stages that still need to run (pipeline order).
#[must_use]
pub fn pending_stages(checkpoint: &CheckpointV1) -> Vec<StageId> {
    StageId::pipeline_order()
        .iter()
        .copied()
        .filter(|s| should_run(*s, checkpoint))
        .collect()
}

/// Clear `from` and every later pipeline stage from the checkpoint (partial regenerate).
///
/// Earlier stages remain completed. Timestamps for cleared stages are removed.
///
/// # Panics
///
/// Debug-only: panics if `from` is missing from [`StageId::pipeline_order`]
/// (should be impossible for any [`StageId`] variant).
pub fn invalidate_from(checkpoint: &mut CheckpointV1, from: StageId) {
    let order = StageId::pipeline_order();
    let start = order
        .iter()
        .position(|s| *s == from)
        .expect("StageId missing from pipeline_order");
    let drop: Vec<StageId> = order[start..].to_vec();
    checkpoint.completed_stages.retain(|s| !drop.contains(s));
    for s in &drop {
        checkpoint.stage_timestamps.remove(s.as_str());
    }
    // Clear stage payloads that belong to identify+ when regenerating those.
    if drop.contains(&StageId::Identify) {
        checkpoint.abstractions = None;
    }
    if drop.contains(&StageId::Relationships) {
        checkpoint.relationships = None;
    }
    if drop.contains(&StageId::Order) {
        checkpoint.order = None;
    }
    if drop.iter().any(|s| {
        matches!(
            s,
            StageId::Chapters | StageId::Setup | StageId::Overview | StageId::Combine
        )
    }) {
        if let Some(so) = checkpoint.stage_outputs.as_mut() {
            for s in &drop {
                if matches!(
                    s,
                    StageId::Chapters | StageId::Setup | StageId::Overview | StageId::Combine
                ) {
                    so.remove(s.as_str());
                }
            }
            if so.entries.is_empty() {
                checkpoint.stage_outputs = None;
            }
        }
    }
}

/// Whether the checkpoint is stale for an incremental resume given the
/// current repository HEAD.
///
/// Per ADR 0013 / issue #226: when the checkpoint recorded a `since_ref`
/// (i.e. the run was an incremental git-diff run), the checkpoint is stale
/// when the HEAD recorded at creation ([`CheckpointV1::git_commit`]) no longer
/// matches `current_head`. A stale checkpoint means new commits landed between
/// the checkpoint's creation and now, so the pipeline should re-diff from
/// `checkpoint.git_commit` to `current_head` rather than skipping crawl.
///
/// - No `since_ref` → not stale (non-incremental run; staleness is irrelevant).
/// - `since_ref` set and `git_commit == current_head` → not stale.
/// - `since_ref` set and `git_commit != current_head` (including when
///   `git_commit` is `None`, e.g. an older checkpoint) → stale.
#[must_use]
pub fn is_checkpoint_stale(checkpoint: &CheckpointV1, current_head: &str) -> bool {
    checkpoint.since_ref.is_some() && checkpoint.git_commit.as_deref() != Some(current_head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decon_core::config::RunConfig;
    use decon_core::{StageId, StageOutputs};

    fn empty_cp() -> CheckpointV1 {
        let cfg = RunConfig::default();
        CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev-a", "t0").unwrap()
    }

    #[test]
    fn empty_runs_all_from_fetch() {
        let cp = empty_cp();
        assert_eq!(next_stage(&cp), Some(StageId::Fetch));
        assert!(should_run(StageId::Fetch, &cp));
        assert_eq!(pending_stages(&cp).len(), StageId::pipeline_order().len());
    }

    #[test]
    fn skip_completed_prefix() {
        let mut cp = empty_cp();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        cp.mark_stage_complete(StageId::DryRun, "t2");
        assert!(!should_run(StageId::Fetch, &cp));
        assert!(!should_run(StageId::DryRun, &cp));
        assert!(should_run(StageId::Identify, &cp));
        assert_eq!(next_stage(&cp), Some(StageId::Identify));
        assert_eq!(
            pending_stages(&cp).first().copied(),
            Some(StageId::Identify)
        );
    }

    #[test]
    fn all_complete_means_nothing_pending() {
        let mut cp = empty_cp();
        for s in StageId::pipeline_order() {
            cp.mark_stage_complete(*s, "t");
        }
        assert_eq!(next_stage(&cp), None);
        assert!(pending_stages(&cp).is_empty());
    }

    #[test]
    fn identity_config_mismatch() {
        let cp = empty_cp();
        let other = RunConfig {
            language: Some("es".into()),
            ..RunConfig::default()
        };
        let err = check_identity(&cp, &other, "rev-a").unwrap_err();
        assert!(matches!(err, ResumeIdentityMismatch::ConfigHash { .. }));
    }

    #[test]
    fn identity_source_mismatch() {
        let cp = empty_cp();
        let err = check_identity(&cp, &RunConfig::default(), "other-rev").unwrap_err();
        assert!(matches!(err, ResumeIdentityMismatch::SourceRevision { .. }));
    }

    #[test]
    fn identity_ok() {
        let cp = empty_cp();
        check_identity(&cp, &RunConfig::default(), "rev-a").unwrap();
    }

    #[test]
    fn invalidate_from_clears_downstream() {
        let mut cp = empty_cp();
        for s in [
            StageId::Fetch,
            StageId::DryRun,
            StageId::Identify,
            StageId::Relationships,
        ] {
            cp.mark_stage_complete(s, "t");
        }
        cp.abstractions = Some(serde_json::json!(["a"]));
        cp.relationships = Some(serde_json::json!({"x": 1}));

        invalidate_from(&mut cp, StageId::Identify);
        assert!(cp.is_stage_complete(StageId::Fetch));
        assert!(cp.is_stage_complete(StageId::DryRun));
        assert!(!cp.is_stage_complete(StageId::Identify));
        assert!(!cp.is_stage_complete(StageId::Relationships));
        assert!(cp.abstractions.is_none());
        assert!(cp.relationships.is_none());
        assert_eq!(next_stage(&cp), Some(StageId::Identify));
    }

    #[test]
    fn invalidate_from_clears_stage_outputs_for_m4_stages() {
        let mut cp = empty_cp();
        for s in [
            StageId::Fetch,
            StageId::DryRun,
            StageId::Identify,
            StageId::Relationships,
            StageId::Order,
            StageId::Chapters,
            StageId::Setup,
            StageId::Overview,
        ] {
            cp.mark_stage_complete(s, "t");
        }
        let mut so = StageOutputs::new();
        so.set(
            StageId::Chapters.as_str(),
            vec![decon_core::StageOutputEntry {
                path: "chapters/01_intro.md".into(),
                sha256: "sha256:abc".into(),
                size: 10,
            }],
        );
        so.set(
            StageId::Setup.as_str(),
            vec![decon_core::StageOutputEntry {
                path: "00_setup.md".into(),
                sha256: "sha256:def".into(),
                size: 5,
            }],
        );
        so.set(
            StageId::Overview.as_str(),
            vec![decon_core::StageOutputEntry {
                path: "00_architecture_overview.md".into(),
                sha256: "sha256:ghi".into(),
                size: 7,
            }],
        );
        cp.stage_outputs = Some(so);

        invalidate_from(&mut cp, StageId::Chapters);
        assert!(!cp.is_stage_complete(StageId::Chapters));
        assert!(!cp.is_stage_complete(StageId::Setup));
        assert!(!cp.is_stage_complete(StageId::Overview));
        assert!(cp.stage_outputs.is_none());
    }

    #[test]
    fn invalidate_from_clears_all_stage_outputs_when_empty() {
        let mut cp = empty_cp();
        for s in [
            StageId::Fetch,
            StageId::DryRun,
            StageId::Chapters,
            StageId::Setup,
        ] {
            cp.mark_stage_complete(s, "t");
        }
        let mut so = StageOutputs::new();
        so.set(
            StageId::Chapters.as_str(),
            vec![decon_core::StageOutputEntry {
                path: "chapters/01.md".into(),
                sha256: "sha256:x".into(),
                size: 1,
            }],
        );
        cp.stage_outputs = Some(so);

        invalidate_from(&mut cp, StageId::Chapters);
        assert!(cp.stage_outputs.is_none());
    }

    #[test]
    fn pipeline_order_covers_all_known_stages() {
        // Guards invalidate_from expect — every StageId must appear in pipeline_order.
        let order = StageId::pipeline_order();
        for s in [
            StageId::Fetch,
            StageId::DryRun,
            StageId::Identify,
            StageId::Relationships,
            StageId::Order,
            StageId::Chapters,
            StageId::Setup,
            StageId::Overview,
            StageId::Combine,
            StageId::Eval,
        ] {
            assert!(order.contains(&s), "missing {s:?}");
        }
    }

    #[test]
    fn resume_matrix_table() {
        // (completed, expected_next)
        let cases: &[(&[StageId], Option<StageId>)] = &[
            (&[], Some(StageId::Fetch)),
            (&[StageId::Fetch], Some(StageId::DryRun)),
            (&[StageId::Fetch, StageId::DryRun], Some(StageId::Identify)),
            (
                &[
                    StageId::Fetch,
                    StageId::DryRun,
                    StageId::Identify,
                    StageId::Relationships,
                    StageId::Order,
                    StageId::Chapters,
                    StageId::Setup,
                    StageId::Overview,
                    StageId::Combine,
                    StageId::Eval,
                ],
                None,
            ),
        ];
        for (done, expect) in cases {
            let mut cp = empty_cp();
            for s in *done {
                cp.mark_stage_complete(*s, "t");
            }
            assert_eq!(next_stage(&cp), *expect, "done={done:?}");
        }
    }

    fn cp_with_git(since: Option<&str>, commit: Option<&str>) -> CheckpointV1 {
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev-a", "t0").unwrap();
        cp.since_ref = since.map(str::to_owned);
        cp.git_commit = commit.map(str::to_owned);
        cp
    }

    #[test]
    fn stale_when_since_set_and_commit_differs() {
        let cp = cp_with_git(Some("v0.5.0"), Some("aaa111"));
        assert!(is_checkpoint_stale(&cp, "bbb222"));
    }

    #[test]
    fn not_stale_when_since_set_and_commit_matches() {
        let cp = cp_with_git(Some("v0.5.0"), Some("aaa111"));
        assert!(!is_checkpoint_stale(&cp, "aaa111"));
    }

    #[test]
    fn not_stale_when_no_since_ref() {
        // Non-incremental run: staleness is irrelevant regardless of commit.
        let cp = cp_with_git(None, Some("aaa111"));
        assert!(!is_checkpoint_stale(&cp, "bbb222"));
    }

    #[test]
    fn stale_when_since_set_but_git_commit_missing() {
        // Old checkpoint (pre-M6-GIT-3) with since_ref somehow set but no
        // git_commit recorded: cannot confirm HEAD matches, so treat as stale.
        let cp = cp_with_git(Some("v0.5.0"), None);
        assert!(is_checkpoint_stale(&cp, "bbb222"));
    }
}
