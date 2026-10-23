//! Pointers from a worker's assignment to other completed branches of its
//! run and problem ([`Assignment::branch_references`]): the harness's own
//! evaluation of each branch and where its files would be, never their text.
//! No workspace file and no model-written note is read here. A recorded
//! verdict describes the submitted snapshot (for a completion solve, the
//! harness's completed version of it); what the pointed-at files hold now is
//! untrusted research material.
//!
//! [`Assignment::branch_references`]: nucleus_explore_interface::Assignment::branch_references

use std::collections::BTreeSet;

use nucleus_explore_interface::{BranchReference, MAX_BRANCH_REFERENCE_BYTES, MAX_BRANCH_REFERENCES};
use nucleus_lean_interface::CheckStatus;

use super::{NodeId, ProblemTree, TreeNode, node_u64};

/// The references for a worker continuing `parent`: checked work first,
/// then newer work; at most one entry per ancestor/descendant chain; never
/// the worker's own ancestors, and never a task whose worker is still
/// `active` (it may still be writing). Equal files on independent branches
/// are kept: their scratch work and notes can differ. A `STATE.md` that does
/// not exist is allowed: a pointer is not a claim that it does.
pub(super) fn references(
    problem: &str,
    tree: &ProblemTree,
    parent: NodeId,
    active: impl Fn(&str) -> bool,
) -> Vec<BranchReference> {
    // These names become path components; the run's problem ids and the
    // harness's task ids already satisfy this, so anything else is refused
    // here rather than trusted.
    if !component(problem) || tree.node(parent).is_none() {
        return Vec::new();
    }
    let mut ancestors = BTreeSet::new();
    let mut cursor = Some(parent);
    while let Some(id) = cursor {
        ancestors.insert(id);
        cursor = tree.node(id).and_then(|node| node.parent);
    }
    let mut candidates: Vec<&TreeNode> = tree
        .nodes()
        .iter()
        .filter(|node| !ancestors.contains(&node.id))
        .filter(|node| node.task.as_deref().is_some_and(|task| component(task) && !active(task)))
        .collect();
    candidates.sort_by(|a, b| quality(b).cmp(&quality(a)).then(b.id.cmp(&a.id)));

    let mut chosen: Vec<&TreeNode> = Vec::new();
    let mut result = Vec::new();
    let mut bytes = 2usize; // The JSON array's brackets.
    for node in candidates {
        if chosen.iter().any(|other| {
            other.task == node.task
                || is_ancestor(tree, other.id, node.id)
                || is_ancestor(tree, node.id, other.id)
        }) {
            continue;
        }
        let Some(task) = node.task.as_deref() else { continue };
        let reference = BranchReference {
            node: node_u64(node.id),
            task: task.to_owned(),
            check_status: node.eval.check.status,
            errors: node.eval.check.errors.len() as u64,
            sorries: node.eval.check.sorries.len() as u64,
            verified: node.eval.verified,
            state_file: format!("../{task}/STATE.md"),
            proof_file: format!("../{task}/{problem}.lean"),
        };
        let Ok(encoded) = serde_json::to_vec(&reference) else { continue };
        if bytes.saturating_add(encoded.len() + 1) > MAX_BRANCH_REFERENCE_BYTES {
            continue;
        }
        bytes += encoded.len() + 1;
        result.push(reference);
        chosen.push(node);
        if result.len() == MAX_BRANCH_REFERENCES {
            break;
        }
    }
    result
}

fn component(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

fn is_ancestor(tree: &ProblemTree, ancestor: NodeId, descendant: NodeId) -> bool {
    let mut cursor = Some(descendant);
    while let Some(id) = cursor {
        if id == ancestor {
            return true;
        }
        cursor = tree.node(id).and_then(|node| node.parent);
    }
    false
}

fn quality(node: &TreeNode) -> u8 {
    if node.eval.verified == Some(true) {
        3
    } else if node.eval.check.errors.is_empty()
        && matches!(node.eval.check.status, CheckStatus::Complete | CheckStatus::Sorries)
    {
        if node.eval.sketch.as_ref().is_some_and(|sketch| sketch.counts.closed > 0) { 2 } else { 1 }
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::{Evaluation, LeanReplies};
    use nucleus_lean_interface::{CheckResponse, OpenGoal};

    fn evaluated(status: CheckStatus, verified: Option<bool>) -> Evaluation {
        let check = CheckResponse {
            status,
            errors: vec![],
            sorries: if status == CheckStatus::Sorries {
                vec![OpenGoal { line: 1, column: 0, goal: "True".into() }]
            } else {
                vec![]
            },
            invalid_reason: None,
            elapsed_ms: 1,
        };
        let mut result = Evaluation::combine(
            "theorem p : True := sorry",
            "theorem p : True := sorry",
            check,
            LeanReplies::default(),
        );
        result.verified = verified;
        result
    }

    fn tree() -> ProblemTree {
        ProblemTree::new("root".into(), evaluated(CheckStatus::Sorries, None), 2)
    }

    fn add(tree: &mut ProblemTree, parent: NodeId, task: &str, file: &str) -> NodeId {
        tree.push(parent, 1, task.into(), file.into(), evaluated(CheckStatus::Sorries, None), None).unwrap()
    }

    #[test]
    fn excludes_own_ancestors_live_workers_and_repeated_paths_but_keeps_independent_work() {
        let mut tree = tree();
        let own = add(&mut tree, 0, "t1", "own ancestor");
        let parent = add(&mut tree, own, "t2", "own parent");
        let other = add(&mut tree, 0, "t3", "other ancestor");
        let newest = add(&mut tree, other, "t4", "other latest");
        add(&mut tree, 0, "t5", "live even though submitted");
        let independent = add(&mut tree, 0, "t6", "same submitted file; scratch work may differ");
        let duplicate = add(&mut tree, 0, "t7", "same submitted file; scratch work may differ");
        let refs = references("p", &tree, parent, |task| task == "t5");
        assert_eq!(
            refs.iter().map(|r| r.node).collect::<Vec<_>>(),
            [node_u64(duplicate), node_u64(independent), node_u64(newest)]
        );
        assert_eq!(refs[2].state_file, "../t4/STATE.md");
        assert_eq!(refs[2].proof_file, "../t4/p.lean");
        assert_eq!(refs[2].check_status, CheckStatus::Sorries);
        assert_eq!(refs[2].sorries, 1);
        assert_eq!(refs[2].verified, None, "a checked partial file is not strictly verified");
    }

    #[test]
    fn prefers_checked_work_but_does_not_infer_verification_or_copy_worker_claims() {
        let mut tree = tree();
        let checked = tree
            .push(
                0,
                1,
                "checked".into(),
                "-- CLAIM: all auxiliary lemmas proved".into(),
                evaluated(CheckStatus::Complete, Some(false)),
                None,
            )
            .unwrap();
        for i in 0..10 {
            tree.push(
                0,
                1,
                format!("bad{i}"),
                format!("bad {i}"),
                evaluated(CheckStatus::Errors, None),
                None,
            )
            .unwrap();
        }
        let refs = references("p", &tree, 0, |_| false);
        assert_eq!(refs.len(), MAX_BRANCH_REFERENCES);
        assert_eq!(refs[0].node, node_u64(checked));
        assert_eq!(refs[0].verified, Some(false), "complete elaboration does not override verify rejection");
        assert_eq!(refs[1].task, "bad9", "recent work breaks ties");
        let encoded = serde_json::to_string(&refs).unwrap();
        assert!(!encoded.contains("CLAIM"), "the index never includes source or model-written notes");
        assert!(encoded.len() <= MAX_BRANCH_REFERENCE_BYTES);
        assert!(refs.iter().all(|r| r.proof_file.ends_with("/p.lean")));
    }

    #[test]
    fn paths_and_encoded_size_remain_bounded_to_this_problems_workspaces() {
        let mut tree = tree();
        add(&mut tree, 0, "../another_problem/t1", "invalid task path");
        add(&mut tree, 0, "t2", "valid");
        assert_eq!(references("p", &tree, 0, |_| false).len(), 1);
        assert!(references("../other/p", &tree, 0, |_| false).is_empty());
        assert!(references("p", &tree, 99, |_| false).is_empty());
        assert!(references(&"p".repeat(MAX_BRANCH_REFERENCE_BYTES), &tree, 0, |_| false).is_empty());
    }

    #[test]
    fn assignment_indexes_only_finished_workers_in_its_authenticated_problem() {
        use crate::{InitialPolicy, Limits, Rollout};
        use nucleus_protocol::{ErrorCode, ProcessId, ResourcePath};

        let limits = Limits {
            w: 2,
            k1: 2,
            fanout: 2,
            calls_per_continuation: 3,
            collection_calls: 5,
            model_calls: 100,
            max_tokens: 0,
            max_usd_micro: 0,
        };
        let mut rollout = Rollout::new(
            "p",
            "root".into(),
            "template".into(),
            evaluated(CheckStatus::Sorries, None),
            limits,
        );
        rollout.plan_round(&mut InitialPolicy).unwrap();
        rollout.open_task("t1", 0, ProcessId(1)).unwrap();
        rollout.open_task("t2", 0, ProcessId(2)).unwrap();
        add(&mut rollout.tree, 0, "t1", "completed candidate");
        let resource = ResourcePath::new(["run", "p", "t2"]).unwrap();
        assert!(rollout.assignment(&resource, ProcessId(2)).unwrap().branch_references.is_empty());
        rollout.worker_exited(ProcessId(1));
        let refs = rollout.assignment(&resource, ProcessId(2)).unwrap().branch_references;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].task, "t1");
        assert_eq!(
            rollout.assignment(&resource, ProcessId(1)).unwrap_err().code,
            ErrorCode::PermissionDenied,
            "another worker's task"
        );
        assert_eq!(
            rollout
                .assignment(&ResourcePath::new(["run", "other", "t2"]).unwrap(), ProcessId(2))
                .unwrap_err()
                .code,
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn completion_verification_is_snapshot_metadata_not_a_workspace_file_certificate() {
        let mut tree = tree();
        let mut eval = evaluated(CheckStatus::Sorries, Some(true));
        eval.spliced = Some("harness-completed proof snapshot".into());
        tree.push(0, 1, "t1".into(), "worker submitted an incomplete file".into(), eval, None).unwrap();
        let refs = references("p", &tree, 0, |_| false);
        assert_eq!(
            refs[0].check_status,
            CheckStatus::Sorries,
            "the original candidate's check survives completion"
        );
        assert_eq!(refs[0].verified, Some(true), "strict verification may concern the completed snapshot");
        assert_eq!(refs[0].proof_file, "../t1/p.lean", "an optional pointer, not a verified copy");
        let encoded = serde_json::to_string(&refs).unwrap();
        assert!(!encoded.contains("harness-completed proof snapshot"));
        assert!(!encoded.contains("worker submitted an incomplete file"));
    }
}
