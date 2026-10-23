//! The session agent's calls: the swarm's status and records, the verdict on
//! a submission (an accept stops the others, a rejection continues the
//! member once), stopping a swarm; and how a swarm ends.
//!
//! A verdict is the session agent's model judgement: a persuasive wrong
//! solution can be accepted by an agent that did not check it.

use nucleus_protocol::{DispatchId, Error, Payload};
use nucleus_sdk::ChannelError;
use nucleus_swarm_interface::{
    MemberState, Outcome, RecordsRequest, StatusRequest, StopRequest, SwarmRecord, VerdictRequest,
    VerdictResponse, valid_id,
};
use serde_json::{Value, json};

use super::{
    Call, Kernel, Phase, Service, Swarm, WAKE_DEADLINE_MS, invalid, latest_recorded, read_records,
    records_path,
};
use crate::swarm::board::{Judged, preview};
use crate::swarm::members;

impl Service {
    /// Stops every member that runs, recording why.
    pub(super) fn stop_members(
        &mut self,
        kernel: &mut Kernel,
        swarm_id: &str,
        why: &str,
    ) -> Result<Vec<String>, ChannelError> {
        let Some(swarm) = self.swarms.get_mut(swarm_id) else { return Ok(Vec::new()) };
        let mut stopped = Vec::new();
        for member in swarm.members.iter_mut().filter(|m| m.state == MemberState::Running) {
            if let Some(process) = member.process.take()
                && let Err(e) = kernel.kill(process)?
            {
                eprintln!("nucleus-swarm: member {} of {swarm_id} could not be stopped: {e}", member.name);
            }
            member.state = MemberState::Stopped;
            stopped.push(member.name.clone());
        }
        for name in &stopped {
            let record = SwarmRecord::Member {
                swarm: swarm_id.to_owned(),
                member: name.clone(),
                state: MemberState::Stopped,
                note: why.to_owned(),
                process: None,
            };
            self.record(swarm_id, record);
        }
        Ok(stopped)
    }

    /// Ends the swarm when nothing runs and nothing waits; `wake`: tell the
    /// session's agent, which is not in a call of its own about it.
    pub(super) fn settle(
        &mut self,
        kernel: &mut Kernel,
        swarm_id: &str,
        wake: bool,
    ) -> Result<(), ChannelError> {
        let Some(swarm) = self.swarms.get(swarm_id) else { return Ok(()) };
        let idle = !swarm.members.iter().any(|m| m.state == MemberState::Running)
            && swarm.board.pending().next().is_none();
        let outcome = match swarm.phase {
            Phase::Running if idle => Outcome::Exhausted,
            Phase::Closing if swarm.board.pending().next().is_none() => Outcome::Deadline,
            _ => return Ok(()),
        };
        self.finish(kernel, swarm_id, outcome, "", wake)
    }

    /// Ends the swarm with `outcome`; `wake`: tell the session's agent.
    pub(super) fn finish(
        &mut self,
        kernel: &mut Kernel,
        swarm_id: &str,
        outcome: Outcome,
        note: &str,
        wake: bool,
    ) -> Result<(), ChannelError> {
        let Some(swarm) = self.swarms.get_mut(swarm_id) else { return Ok(()) };
        if matches!(swarm.phase, Phase::Ended(_)) {
            return Ok(());
        }
        swarm.phase = Phase::Ended(outcome);
        let waiting: Vec<DispatchId> = swarm
            .queue
            .drain(..)
            .map(|r| r.dispatch)
            .chain(swarm.checking.take().map(|(_, r)| r.dispatch))
            .collect();
        let Ok(session) = swarm.session_resource() else { return Ok(()) };
        let record = SwarmRecord::Ended {
            swarm: swarm_id.to_owned(),
            outcome,
            note: note.to_owned(),
            calls: swarm.calls(),
            reserved_calls: swarm.reserved,
            checks: swarm.checks,
        };
        let outcome_name = swarm.state_name();
        self.unmark(swarm_id);
        for dispatch in waiting {
            kernel.reply(dispatch, Err(invalid("the swarm ended")))?;
        }
        self.record(swarm_id, record);
        let request = nucleus_conversation_interface::UltramathEndedRequest { swarm: swarm_id.to_owned() };
        match kernel.submit(
            self.authority.ended,
            nucleus_conversation_interface::ACTION_ULTRAMATH_ENDED,
            &session,
            &request,
            Some(WAKE_DEADLINE_MS),
        )? {
            Ok(id) => {
                self.calls.insert(id, Call::Ended(swarm_id.to_owned()));
            }
            Err(e) => eprintln!(
                "nucleus-swarm: completion of {swarm_id} could not be added to the conversation: {e}"
            ),
        }
        if wake && outcome != Outcome::Solved {
            let text = format!(
                "Swarm {swarm_id} ended without an accepted solution ({outcome_name}). Read the board with \
                 ultramath_status and its records with ultramath_records, and tell the person what they show."
            );
            if let Err(e) = self.wake(kernel, &session, text, Call::Wake(swarm_id.to_owned()))? {
                eprintln!("nucleus-swarm: the session of swarm {swarm_id} could not be woken: {e}");
            }
        }
        Ok(())
    }

    pub(super) fn session_swarm(&mut self, project: &str, session: &str) -> Result<&mut Swarm, Error> {
        let id = self
            .latest
            .get(&(project.to_owned(), session.to_owned()))
            .ok_or_else(|| invalid("this session has started no swarm"))?;
        self.swarms.get_mut(id).ok_or_else(|| invalid("this session's swarm is gone"))
    }

    pub(super) fn status(
        &mut self,
        project: &str,
        session: &str,
        payload: &Payload,
    ) -> Result<Payload, Error> {
        let request = payload.decode::<StatusRequest>()?;
        let swarm = self.session_swarm(project, session)?;
        if let Some(direction) = request.direction {
            let page = swarm.board.page(&direction)?;
            let waiting = swarm.board.pending().find(|s| s.direction == direction).cloned();
            return Payload::encode(&json!({"swarm": swarm.id, "direction": page, "submission": waiting}));
        }
        let members: Vec<Value> = swarm
            .members
            .iter()
            .map(|m| {
                json!({"member": m.name, "state": m.state, "runs": m.runs,
                       "direction": swarm.board.held_by(&m.name).map(|d| d.id.clone())
                           .or_else(|| swarm.board.pending_of(&m.name).map(|s| s.direction.clone()))})
            })
            .collect();
        let status = json!({
            "swarm": swarm.id,
            "state": swarm.state_name(),
            "task": preview(&swarm.task, 1500),
            "dir": format!(".ultramath/{}", swarm.id),
            "model": swarm.model,
            "reasoning_effort": swarm.effort,
            "limits": {"members": swarm.members.len(), "member_steps": swarm.member_steps, "budget": swarm.budget,
                       "reserved": swarm.reserved, "duplicate_checks": swarm.checks,
                       "minutes_left": swarm.minutes_left()},
            "members": members,
            "waiting_for_verdict": swarm.board.pending_previews(),
            "board": swarm.board.overview(),
            "records": {"written": swarm.next_seq, "unwritten": swarm.unwritten},
        });
        Payload::encode(&status)
    }

    /// A page of one of this session's swarms' records: the one it names, or
    /// the session's latest.
    pub(super) fn records(
        &mut self,
        project: &str,
        session: &str,
        payload: &Payload,
    ) -> Result<Payload, Error> {
        let request = payload.decode::<RecordsRequest>()?;
        let limit = request.validate()?;
        let swarm = match request.swarm {
            Some(swarm) if valid_id(&swarm) => swarm,
            Some(_) => return Err(invalid("swarm is not a swarm's id")),
            None => latest_recorded(&self.options.state, project, session)
                .ok_or_else(|| invalid("this session has started no swarm"))?,
        };
        let path = records_path(&self.options.state, project, session, &swarm);
        Payload::encode(&read_records(&path, &swarm, request.after, limit)?)
    }

    pub(super) fn verdict(
        &mut self,
        kernel: &mut Kernel,
        project: &str,
        session: &str,
        payload: &Payload,
    ) -> Result<Result<Payload, Error>, ChannelError> {
        let request = match payload.decode::<VerdictRequest>().and_then(|r| r.validate().map(|()| r)) {
            Ok(request) => request,
            Err(e) => return Ok(Err(e)),
        };
        let swarm = match self.session_swarm(project, session) {
            Ok(swarm) if swarm.running() => swarm,
            Ok(swarm) => return Ok(Err(invalid(format!("swarm {} has ended", swarm.id)))),
            Err(e) => return Ok(Err(e)),
        };
        let judged = match swarm.board.judge(&request.member, request.accept, &request.reason) {
            Ok(judged) => judged,
            Err(e) => return Ok(Err(e)),
        };
        let swarm_id = swarm.id.clone();
        let direction = match &judged {
            Judged::Solved { direction } | Judged::Retry { direction } | Judged::Dead { direction } => {
                direction.clone()
            }
        };
        self.record(
            &swarm_id,
            SwarmRecord::Verdict {
                swarm: swarm_id.clone(),
                member: request.member.clone(),
                direction: direction.clone(),
                accepted: request.accept,
                reason: request.reason.clone(),
            },
        );
        let member = request.member.clone();
        let outcome = match judged {
            Judged::Solved { direction } => {
                self.direction_changed(&swarm_id, &direction);
                if let Some(swarm) = self.swarms.get_mut(&swarm_id)
                    && let Some(m) = swarm.members.iter_mut().find(|m| m.name == member)
                {
                    m.state = MemberState::Ended;
                }
                let stopped = self.stop_members(kernel, &swarm_id, "the swarm is solved")?;
                self.finish(
                    kernel,
                    &swarm_id,
                    Outcome::Solved,
                    &format!("{member} solved it under {direction}"),
                    false,
                )?;
                format!(
                    "Accepted. {} other members were stopped ({}). The swarm is solved: tell the person.",
                    stopped.len(),
                    stopped.join(", ")
                )
            }
            Judged::Retry { direction } => {
                let task = self.swarms.get(&swarm_id).map(|s| {
                    members::retry_task(&direction, &request.reason, s.member_steps, s.minutes_left())
                });
                match self.continue_member(kernel, &swarm_id, &member, &task.unwrap_or_default(), true)? {
                    Ok(()) => {
                        format!("Rejected. {member} continues under {direction} with your reason, once.")
                    }
                    Err(why) => format!("Rejected. {member} cannot continue: {why}."),
                }
            }
            Judged::Dead { direction } => {
                self.direction_changed(&swarm_id, &direction);
                let task = self.swarms.get(&swarm_id).map(|s| {
                    members::dead_task(&direction, &request.reason, s.member_steps, s.minutes_left())
                });
                match self.continue_member(kernel, &swarm_id, &member, &task.unwrap_or_default(), false)? {
                    Ok(()) => format!(
                        "Rejected a second time: direction {direction} is dead. {member} must register another."
                    ),
                    Err(why) => format!(
                        "Rejected a second time: direction {direction} is dead. {member} stops: {why}."
                    ),
                }
            }
        };
        self.settle(kernel, &swarm_id, false)?;
        let running = self.swarms.get(&swarm_id).is_some_and(Swarm::running);
        let outcome = if running {
            outcome
        } else {
            let state = self.swarms.get(&swarm_id).map(Swarm::state_name).unwrap_or_default();
            format!("{outcome} The swarm has ended ({state}): tell the person what its board shows.")
        };
        Ok(Payload::encode(&VerdictResponse { outcome, running }))
    }

    /// Starts `name` again on its transcript with `task`; on failure it ends,
    /// and the reason is returned and recorded.
    pub(super) fn continue_member(
        &mut self,
        kernel: &mut Kernel,
        swarm_id: &str,
        name: &str,
        task: &str,
        holding: bool,
    ) -> Result<Result<(), String>, ChannelError> {
        let Some(mut swarm) = self.swarms.remove(swarm_id) else {
            return Ok(Err("the swarm is gone".into()));
        };
        // Its previous run ended with its submission; if it lingers, it goes.
        if let Some(member) = swarm.members.iter_mut().find(|m| m.name == name)
            && let Some(process) = member.process.take()
        {
            let _ = kernel.kill(process)?;
        }
        let started = self.run_member(kernel, &mut swarm, name, task, holding, true)?;
        let id = swarm.id.clone();
        if started.is_err()
            && let Some(member) = swarm.members.iter_mut().find(|m| m.name == name)
        {
            member.state = MemberState::Ended;
        }
        let process = swarm.members.iter().find(|m| m.name == name).and_then(|m| m.process);
        self.swarms.insert(id.clone(), swarm);
        match &started {
            Ok(()) => {
                let note = "continued with the judge's reason".to_owned();
                let state = MemberState::Running;
                let member = name.to_owned();
                self.record(&id, SwarmRecord::Member { swarm: id.clone(), member, state, note, process });
            }
            // It cannot go on: what it still holds is given up, with why.
            Err(e) => self.member_ended(&id, name, &format!("it could not continue: {}", e.message)),
        }
        Ok(started.map_err(|e| e.message))
    }

    pub(super) fn stop(
        &mut self,
        kernel: &mut Kernel,
        project: &str,
        session: &str,
        payload: &Payload,
    ) -> Result<Result<Payload, Error>, ChannelError> {
        let request = match payload.decode::<StopRequest>().and_then(|r| r.validate().map(|()| r)) {
            Ok(request) => request,
            Err(e) => return Ok(Err(e)),
        };
        let swarm_id = match self.session_swarm(project, session) {
            Ok(swarm) if swarm.running() => swarm.id.clone(),
            Ok(swarm) => return Ok(Err(invalid(format!("swarm {} has ended", swarm.id)))),
            Err(e) => return Ok(Err(e)),
        };
        let stopped = self.stop_members(kernel, &swarm_id, "the session's agent stopped the swarm")?;
        if let Some(swarm) = self.swarms.get_mut(&swarm_id) {
            swarm.board.drop_pending();
        }
        let note = preview(&request.reason, 900);
        self.finish(kernel, &swarm_id, Outcome::Stopped, &note, false)?;
        Ok(Payload::encode(&json!({"stopped": stopped})))
    }
}
