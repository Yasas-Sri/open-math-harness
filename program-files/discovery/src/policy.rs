//! The exploration policy's side of the harness: what a policy sees of a
//! discovery tree ([`View`], with each node's [`Observation`]), the [`Policy`]
//! that chooses which nodes to CONTINUE, and [`process`], which drives a policy
//! that lives in another process over a line protocol.
//!
//! The single atomic operation is CONTINUE(v): generate one child of node v. A
//! policy sees the revealed tree and the nodes it may continue, and returns a
//! batch to CONTINUE (at most `w` of them), or stops.
pub mod process;

/// Identifier of a node within one discovery tree.
pub type NodeId = usize;

/// A task score. Larger is better, following PutnamBench's convention.
pub type Score = f64;

/// What the harness observed about a node beyond its score: Lean's verdict
/// on its file and the worker's spend, as recorded. A policy is shown it
/// (protocol v1, optional per-node `check` and `tokens`) so it can tell a
/// restated theorem from a decomposed one at equal score, or a node whose
/// continuations keep failing.
/// The root has a verdict and no worker.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Observation {
    /// `invalid`, `errors`, `sorries` or `complete`.
    pub status: String,
    pub errors: u64,
    pub sorries: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// The sketch analysis of a `sorries` node (task-scoring protocol v3),
    /// when the record carries one.
    pub sketch: Option<SketchObservation>,
    /// The error observations of an `errors` node, when recorded.
    pub errors_obs: Option<ErrorsObs>,
    /// How the node's verdict moved against its parent's, when recorded.
    pub delta_vs_parent: Option<(i64, i64)>,
    /// Why `verify` rejected a complete candidate, when it did.
    pub verify_rejection: Option<String>,
}

/// What the `lean` service's `analyze` decided about a `sorries` node, as the
/// harness recorded it (protocol v3 observations; [`process`] documents the
/// request fields).
#[derive(Clone, Debug, PartialEq)]
pub struct SketchObservation {
    /// Some open leaf is the theorem in disguise; `None` when the analysis
    /// did not run (the node scored the base).
    pub hollow: Option<bool>,
    /// Some open leaf is `False`: a by-contradiction sketch, which the
    /// hollow rule treats as `sorry False`.
    pub false_leaf: bool,
    /// Some open leaf is the frozen target verbatim.
    pub restates_root: bool,
    /// Closed `have`/`obtain` steps that count (non-trivial, used).
    pub closed: u64,
    /// Open leaves.
    pub open: u64,
    /// Remaining sorries the completion suite closed (partial closures earn
    /// nothing; all of them closing made the node a solve instead).
    pub auto_closable: u64,
    /// The analysis could not run to the end.
    pub analysis_failed: bool,
}

/// What the harness recorded about an `errors` node's diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorsObs {
    /// At most three errors.
    pub repairable: bool,
    /// One class per error (a multiset).
    pub classes: Vec<String>,
    /// Where the first error falls in the proof body, 0 (top) to 1 (end).
    pub first_error_fraction: Option<f64>,
}

/// What a policy is allowed to observe: the revealed nodes' scores, and which
/// nodes may be continued now, best score first. A policy
/// never sees the outcome of a node that has not been revealed. The revealed
/// tree's shape (each node's parent, hence its depth) travels beside the view,
/// see [`Policy::decide_with_parents`].
#[derive(Clone, Debug)]
pub struct View {
    /// (node id, score) for every revealed node.
    pub revealed: Vec<(NodeId, Score)>,
    /// Continuable nodes, ordered best score first.
    pub continuable: Vec<NodeId>,
    /// What was observed about each revealed node, indexed like `revealed`
    /// (`None` where the record carries nothing beyond the score).
    pub observations: Vec<Option<Observation>>,
}

/// An exploration policy: given the revealed view and `w` parallel workers,
/// choose a batch of continuable nodes (at most `w`), or return empty to stop.
pub trait Policy {
    /// Decide given, besides the view, the revealed tree's shape: `parents[i]`
    /// is the parent of `view.revealed[i]` (`None` for the root), from which
    /// depth follows. The discovery harness calls this each decision round.
    fn decide_with_parents(&mut self, view: &View, parents: &[Option<NodeId>], w: usize) -> Vec<NodeId>;
}
