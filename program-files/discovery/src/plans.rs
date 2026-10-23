//! Live research proposals of one rollout (explore `publish_plan` and
//! `read_plans`): the board the live workers of one run and problem
//! coordinate on. It is scheduling advice between untrusted workers, never
//! verification and never a reservation of authority. It lives in memory
//! only and nothing about it is journaled: a proposal goes when its worker
//! submits, exits or times out, the board is cleared when its round closes,
//! and a resumed run starts with an empty board.
//!
//! The board holds at most one proposal per live worker, so at most the
//! task's `w`; [`explore::MAX_PLAN_BOARD_BYTES`] bounds it in bytes as well,
//! and a publication that would overflow it is refused rather than hiding
//! earlier proposals. A [`Rollout`] serves its board only to the live,
//! unsubmitted worker of each task, into that worker's own slot.
//!
//! A compromised worker can fill the board with noise within these bounds
//! or withhold its plan. It cannot publish as another worker (the owner is
//! the task in its resource, bound to the process started for that task),
//! read another problem's or run's board, or keep a proposal after it exits.

use std::collections::BTreeMap;

use nucleus_explore_interface::{self as explore, PlanBoard, PublishPlan, ResearchPlan};
use nucleus_protocol::{Error, ErrorCode, Payload, ProcessId, ResourcePath};

use super::{Rollout, invalid};

impl Rollout {
    /// Whether this rollout's workers coordinate on a plan board: a round
    /// of one has nobody to coordinate with.
    #[must_use]
    pub fn coordinates(&self) -> bool {
        self.limits.w >= 2
    }

    /// `publish_plan` or `read_plans` of `caller` on `resource`, `[key, problem,
    /// task]`, at `now_ms`.
    pub(super) fn serve_plans(
        &mut self,
        action: &str,
        resource: &ResourcePath,
        caller: ProcessId,
        payload: &Payload,
        now_ms: u64,
    ) -> Result<Payload, Error> {
        let board = if action == explore::ACTION_PUBLISH_PLAN {
            let request = payload.decode::<PublishPlan>()?;
            self.publish_plan_at(resource, caller, request, now_ms)?
        } else {
            payload.decode::<explore::ReadPlans>()?;
            self.read_plans(resource, caller)?
        };
        Payload::encode(&board)
    }

    /// The board as the authenticated, live, unsubmitted worker of the
    /// resource's task sees it.
    pub fn read_plans(&self, resource: &ResourcePath, caller: ProcessId) -> Result<PlanBoard, Error> {
        let task = self.planner(resource, caller)?;
        Ok(self.plans.read(&task))
    }

    /// Publishes into that worker's own slot; the service, never the
    /// payload, supplies the time and the order.
    fn publish_plan_at(
        &mut self,
        resource: &ResourcePath,
        caller: ProcessId,
        request: PublishPlan,
        now_ms: u64,
    ) -> Result<PlanBoard, Error> {
        let task = self.planner(resource, caller)?;
        self.plans.publish(&task, request, now_ms)
    }

    fn planner(&self, resource: &ResourcePath, caller: ProcessId) -> Result<String, Error> {
        if !self.coordinates() {
            return Err(Error::new(ErrorCode::PermissionDenied, "a task with w 1 has no plan board"));
        }
        let (task, c) = self.continuation(resource, caller)?;
        if c.submitted {
            return Err(invalid("that task already submitted"));
        }
        Ok(task)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Plans {
    current: BTreeMap<String, ResearchPlan>,
    sequence: u64,
    last_ms: u64,
}

impl Plans {
    /// Publishes `request` as the proposal of `task`, the authenticated
    /// owner. The service supplies the time, the order and the revision;
    /// publishing the same texts again changes nothing.
    pub(super) fn publish(
        &mut self,
        task: &str,
        request: PublishPlan,
        now_ms: u64,
    ) -> Result<PlanBoard, Error> {
        request.validate()?;
        let previous = self.current.get(task);
        if previous.is_some_and(|p| {
            p.goal == request.goal && p.approach == request.approach && p.next_step == request.next_step
        }) {
            return Ok(self.read(task));
        }
        let exhausted = || Error::new(ErrorCode::ServiceError, "research plan ordering exhausted");
        let sequence = self.sequence.checked_add(1).ok_or_else(exhausted)?;
        let revision = previous.map_or(Some(1), |p| p.revision.checked_add(1)).ok_or_else(exhausted)?;
        let published_at_ms = now_ms.max(self.last_ms);
        let plan = ResearchPlan {
            task: task.to_owned(),
            published_at_ms,
            sequence,
            revision,
            goal: request.goal,
            approach: request.approach,
            next_step: request.next_step,
        };
        let mut all: Vec<_> = self.current.values().filter(|p| p.task != task).cloned().collect();
        all.push(plan.clone());
        // No own publication is the largest reply: `own: null` and every
        // proposal in `earlier`.
        let board = PlanBoard { own: None, earlier: all };
        let bytes =
            serde_json::to_vec(&board).map_err(|e| Error::new(ErrorCode::ServiceError, e.to_string()))?;
        if bytes.len() > explore::MAX_PLAN_BOARD_BYTES {
            return Err(Error::new(ErrorCode::InvalidInput, "the research plan board is full"));
        }
        self.current.insert(task.to_owned(), plan);
        self.sequence = sequence;
        self.last_ms = published_at_ms;
        Ok(self.read(task))
    }

    /// The board as `task` sees it: its own proposal and those published
    /// before it (all of them before its first publication).
    pub(super) fn read(&self, task: &str) -> PlanBoard {
        let own = self.current.get(task).cloned();
        let mut earlier: Vec<_> = self
            .current
            .values()
            .filter(|p| p.task != task && own.as_ref().is_none_or(|own| p.sequence < own.sequence))
            .cloned()
            .collect();
        earlier.sort_by_key(|p| p.sequence);
        PlanBoard { own, earlier }
    }

    pub(super) fn release(&mut self, task: &str) {
        self.current.remove(task);
    }

    pub(super) fn clear(&mut self) {
        self.current.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::tests::{limits, payload, rollout, submission};
    use crate::{Limits, Served};

    fn proposal(goal: &str) -> PublishPlan {
        PublishPlan { goal: goal.into(), approach: "factorization".into(), next_step: "test n=2".into() }
    }

    #[test]
    fn concurrent_timestamps_and_backwards_clocks_keep_a_total_order() {
        let mut plans = Plans::default();
        let a = plans.publish("a", proposal("lemma"), 100).unwrap();
        let b = plans.publish("b", proposal("lemma"), 100).unwrap();
        let c = plans.publish("c", proposal("lemma"), 90).unwrap();
        assert_eq!(a.own.unwrap().sequence, 1);
        assert_eq!(b.own.as_ref().unwrap().sequence, 2);
        assert_eq!(b.earlier.iter().map(|p| p.task.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(c.own.as_ref().unwrap().published_at_ms, 100);
        assert_eq!(c.own.unwrap().sequence, 3);
        assert_eq!(c.earlier.iter().map(|p| p.task.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn changed_plans_move_back_but_exact_retries_are_idempotent() {
        let mut plans = Plans::default();
        let first = plans.publish("a", proposal("lemma"), 100).unwrap();
        plans.publish("b", proposal("lemma"), 101).unwrap();
        assert_eq!(plans.publish("a", proposal("lemma"), 102).unwrap(), first);
        let revised = plans.publish("a", proposal("weaker lemma"), 102).unwrap();
        let own = revised.own.unwrap();
        assert_eq!((own.sequence, own.revision, own.published_at_ms), (3, 2, 102));
        assert_eq!(revised.earlier.iter().map(|p| p.task.as_str()).collect::<Vec<_>>(), ["b"]);
        assert!(plans.read("b").earlier.is_empty(), "the old revision lost its priority");
        let observer = plans.read("not-yet-published");
        assert!(observer.own.is_none());
        assert_eq!(observer.earlier.iter().map(|p| p.task.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        plans.release("b");
        assert!(plans.read("a").earlier.is_empty());
        plans.clear();
        assert!(plans.read("a").own.is_none());
        assert_eq!(plans.publish("c", proposal("new"), 1).unwrap().own.unwrap().sequence, 4);
    }

    #[test]
    fn exhaustion_and_invalid_changes_leave_the_previous_plan_intact() {
        let mut plans = Plans::default();
        let first = plans.publish("a", proposal("lemma"), 100).unwrap();
        let mut invalid = proposal("new");
        invalid.approach = " ".into();
        assert!(plans.publish("a", invalid, 101).is_err());
        assert_eq!(plans.read("a"), first);
        plans.sequence = u64::MAX;
        assert!(plans.publish("a", proposal("new"), 102).is_err());
        assert_eq!(plans.read("a"), first);
    }

    #[test]
    fn board_capacity_counts_json_escaping_and_never_silently_drops_earlier_plans() {
        let mut plans = Plans::default();
        // A quote is one byte of text and two of JSON.
        let escaped =
            PublishPlan { goal: "\"".repeat(512), approach: "\"".repeat(1024), next_step: "\"".repeat(512) };
        let mut accepted = 0;
        for id in 0..64 {
            let task = format!("t{id}");
            let previous = plans.clone();
            if plans.publish(&task, escaped.clone(), 100).is_err() {
                assert_eq!(plans, previous, "a refused publication never changes ordering or live plans");
                break;
            }
            accepted += 1;
        }
        assert!(accepted > 16 && accepted < 64, "{accepted}");
        let complete = plans.read("new-worker");
        assert_eq!(complete.earlier.len(), accepted);
        assert!(serde_json::to_vec(&complete).unwrap().len() <= explore::MAX_PLAN_BOARD_BYTES);
    }

    #[test]
    fn live_plans_are_owner_bound_isolated_and_released_at_every_terminal_transition() {
        let mut run = rollout(0.1, limits());
        let a = ResourcePath::new(["run", "demo", "a"]).unwrap();
        let b = ResourcePath::new(["run", "demo", "b"]).unwrap();
        run.open_task("a", 0, ProcessId(10)).unwrap();
        run.open_task("b", 0, ProcessId(11)).unwrap();
        assert_eq!(run.read_plans(&a, ProcessId(11)).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(
            run.publish_plan_at(&a, ProcessId(11), proposal("lemma"), 1).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        let other_problem = ResourcePath::new(["other", "a"]).unwrap();
        assert!(run.read_plans(&other_problem, ProcessId(10)).is_err());
        run.publish_plan_at(&a, ProcessId(10), proposal("lemma"), 100).unwrap();
        assert_eq!(run.read_plans(&b, ProcessId(11)).unwrap().earlier.len(), 1);
        // Another run's rollout of the same problem has its own board.
        let mut other_run = rollout(0.1, limits());
        other_run.open_task("b", 0, ProcessId(12)).unwrap();
        assert!(other_run.read_plans(&b, ProcessId(12)).unwrap().earlier.is_empty());
        assert!(other_run.read_plans(&b, ProcessId(11)).is_err());
        run.worker_exited(ProcessId(10));
        assert!(run.read_plans(&b, ProcessId(11)).unwrap().earlier.is_empty());
        assert!(run.publish_plan_at(&a, ProcessId(10), proposal("lemma"), 101).is_err());
        run.publish_plan_at(&b, ProcessId(11), proposal("lemma"), 102).unwrap();
        run.record_submission("b", &submission("proof"));
        assert!(run.plans.read("b").own.is_none());
        assert!(run.read_plans(&b, ProcessId(11)).is_err());
        assert!(run.publish_plan_at(&b, ProcessId(11), proposal("lemma"), 103).is_err());
        let solved_elsewhere: fn(&mut Rollout) -> Vec<ProcessId> = |r| r.stop_for_solve("another");
        for close in [Rollout::time_out, solved_elsewhere] {
            let mut run = rollout(0.1, limits());
            run.open_task("a", 0, ProcessId(10)).unwrap();
            run.publish_plan_at(&a, ProcessId(10), proposal("lemma"), 100).unwrap();
            close(&mut run);
            assert!(run.plans.read("a").own.is_none());
            assert!(run.read_plans(&a, ProcessId(10)).is_err());
        }
        let mut run = rollout(0.1, limits());
        run.open_task("a", 0, ProcessId(10)).unwrap();
        run.publish_plan_at(&a, ProcessId(10), proposal("lemma"), 100).unwrap();
        run.finish_round();
        assert!(run.plans.read("x").earlier.is_empty(), "a closed round leaves no board behind");
        // A round of one has nobody to coordinate with.
        let mut alone = rollout(0.1, Limits { w: 1, ..limits() });
        alone.open_task("a", 0, ProcessId(10)).unwrap();
        assert_eq!(alone.read_plans(&a, ProcessId(10)).unwrap_err().code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn explore_plan_dispatches_reject_forged_metadata_without_changing_the_board() {
        let mut run = rollout(0.1, limits());
        run.open_task("a", 0, ProcessId(10)).unwrap();
        let resource = ResourcePath::new(["run", "demo", "a"]).unwrap();
        let request = serde_json::json!({"goal":"lemma", "approach":"roots", "next_step":"test"});
        let Served::Reply(Ok(reply)) =
            run.serve(explore::ACTION_PUBLISH_PLAN, &resource, ProcessId(10), &payload(&request))
        else {
            panic!("publish a live worker's plan");
        };
        let before = reply.decode::<PlanBoard>().unwrap();
        assert_eq!(before.own.as_ref().unwrap().task, "a");
        for action in [explore::ACTION_PUBLISH_PLAN, explore::ACTION_READ_PLANS] {
            let mut forged =
                if action == explore::ACTION_READ_PLANS { serde_json::json!({}) } else { request.clone() };
            forged["published_at_ms"] = serde_json::json!(0);
            assert!(matches!(
                run.serve(action, &resource, ProcessId(10), &payload(&forged)),
                Served::Reply(Err(_))
            ));
        }
        assert_eq!(run.read_plans(&resource, ProcessId(10)).unwrap(), before);
    }
}
