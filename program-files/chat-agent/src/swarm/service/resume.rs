//! Resuming a swarm a restart interrupted (`resume`) restores its context
//! and nothing more: it is rebuilt from its records, which hold its start, its
//! members' runs, its board and its model calls, and each member that was
//! working when it stopped is started again on its transcript. A call that ran
//! when the restart came has no result there; the member gives it one saying
//! it did not return, its outcome unknown (`crate::restore::transcript`), and
//! redoes what it needs itself. Its reports waiting for a verdict wait again.
//! It keeps its deadline (its start's, or its last resumption's) and runs
//! until then; once that has passed, or less than two minutes of it are left,
//! `resume` needs `minutes`, and it runs that long from now.
//!
//! A swarm started before the `.ultramath/` layout (its records' `dir` is
//! `swarms/<id>`) is not resumed: its members' transcripts are not where a
//! member is started now.
//!
//! The model calls it carries, and counts against its budget, are what its
//! last usage record says. Usage is recorded at most once a minute, so the
//! calls made after that record are not counted, by design: the budget is
//! not rebuilt more exactly than the records keep it.
//!
//! Only a swarm whose last end is [`Outcome::Interrupted`] resumes, once per
//! interruption: a resumed one runs again in this process, and a later restart
//! interrupts it again.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use nucleus_projects_interface::Dir;
use nucleus_protocol::{Error, Payload};
use nucleus_sdk::ChannelError;
use nucleus_swarm_interface::{MemberState, Outcome, ResumeRequest, ResumeResponse, SwarmRecord, valid_id};

use super::{
    Kernel, Member, Phase, Service, Swarm, failed, invalid, last_record, next_seq_of, now_ms,
    read_swarm_records, records_dir, records_path,
};
use crate::swarm::board::Board;
use crate::swarm::members;

/// Less time left than this needs `minutes`: a member's run would barely start.
const LEAST_LEFT_MS: u64 = 2 * 60_000;

/// The model calls a swarm's records say it made: the most any usage, end or
/// resumption record says (each counts all of them so far).
pub(super) fn calls_made(records: &[(u64, SwarmRecord)]) -> u64 {
    records
        .iter()
        .filter_map(|(_, record)| match record {
            SwarmRecord::Usage { calls, .. }
            | SwarmRecord::Ended { calls, .. }
            | SwarmRecord::Resumed { calls, .. } => Some(*calls),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Whether records say their swarm was interrupted and not resumed since.
fn interrupted(records: &[(u64, SwarmRecord)]) -> bool {
    records.iter().rev().find_map(|(_, record)| match record {
        SwarmRecord::Ended { outcome, .. } => Some(*outcome == Outcome::Interrupted),
        SwarmRecord::Resumed { .. } => Some(false),
        _ => None,
    }) == Some(true)
}

/// The swarm's deadline, UNIX milliseconds: its start's, or its last
/// resumption's.
fn deadline_at_ms(records: &[(u64, SwarmRecord)]) -> u64 {
    records
        .iter()
        .rev()
        .find_map(|(_, record)| match record {
            SwarmRecord::Started { deadline_at_ms, .. } | SwarmRecord::Resumed { deadline_at_ms, .. } => {
                Some(*deadline_at_ms)
            }
            _ => None,
        })
        .unwrap_or(0)
}

impl Service {
    /// The records of the session's swarm `id`, when it was interrupted.
    fn interrupted_records(
        &self,
        project: &str,
        session: &str,
        id: &str,
    ) -> Result<Vec<(u64, SwarmRecord)>, Error> {
        if self.swarms.get(id).is_some_and(|s| s.running() && s.project == project && s.session == session) {
            return Err(invalid(format!("swarm {id} runs")));
        }
        let path = records_path(&self.options.state, project, session, id);
        let records = read_swarm_records(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => invalid(format!("this session has no swarm {id}")),
            _ => failed(format!("the records of swarm {id}: {e}")),
        })?;
        if !interrupted(&records) {
            return Err(invalid(format!(
                "swarm {id} was not interrupted: only a swarm a restart interrupted resumes"
            )));
        }
        Ok(records)
    }

    /// The session's only interrupted swarm, with its records. Whether a
    /// swarm is interrupted is read from the end of its records file alone:
    /// nothing is written to it after its interruption until it is resumed.
    fn only_interrupted(
        &self,
        project: &str,
        session: &str,
    ) -> Result<(String, Vec<(u64, SwarmRecord)>), Error> {
        let directory = records_dir(&self.options.state, project, session);
        let mut found: Vec<String> = std::fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.strip_suffix(".jsonl").map(str::to_owned))
            .filter(|id| valid_id(id))
            .filter(|id| {
                let last = last_record(&records_path(&self.options.state, project, session, id));
                matches!(last, Ok(Some(SwarmRecord::Ended { outcome: Outcome::Interrupted, .. })))
            })
            .collect();
        found.sort_by_key(|id| (id.len(), id.clone()));
        match found.as_slice() {
            [] => Err(invalid("this session has no interrupted swarm")),
            [id] => Ok((id.clone(), self.interrupted_records(project, session, id)?)),
            several => Err(invalid(format!(
                "this session has {} interrupted swarms ({}): name one with swarm",
                several.len(),
                several.join(", ")
            ))),
        }
    }

    /// Swarm `id` as `records` leave it, to run for `left_ms` from now: its
    /// members' states and runs, its board, its model calls so far.
    fn rebuild(
        &self,
        project: &str,
        session: &str,
        id: &str,
        records: &[(u64, SwarmRecord)],
        left_ms: u64,
    ) -> Result<Swarm, Error> {
        let started = records.iter().find_map(|(_, record)| match record {
            SwarmRecord::Started { task, members, model, reasoning_effort, member_steps, budget, .. } => {
                Some((task, members, model, *reasoning_effort, *member_steps, *budget))
            }
            _ => None,
        });
        let Some((task, names, model, effort, member_steps, budget)) = started else {
            return Err(failed(format!("the records of swarm {id} do not say how it started")));
        };
        let Some((model, model_resource, model_handle)) =
            self.authority.models.iter().find(|(offered, _, _)| offered == model).cloned()
        else {
            return Err(invalid(format!("swarm {id} ran {model}, which is no longer offered to swarms")));
        };
        let mut states: BTreeMap<&str, (MemberState, u32)> =
            names.iter().map(|name| (name.as_str(), (MemberState::Ended, 0))).collect();
        for (_, record) in records {
            if let SwarmRecord::Member { member, state, .. } = record
                && let Some((last, runs)) = states.get_mut(member.as_str())
            {
                *last = *state;
                if *state == MemberState::Running {
                    *runs += 1;
                }
            }
        }
        let carried = calls_made(records);
        let root = Dir::repository(project)?.resource();
        Ok(Swarm {
            id: id.to_owned(),
            project: project.to_owned(),
            session: session.to_owned(),
            root,
            task: task.clone(),
            model,
            model_resource,
            model_handle,
            effort,
            members: names
                .iter()
                .map(|name| {
                    let (state, runs) = states[name.as_str()];
                    Member { name: name.clone(), process: None, state, runs, calls: 0, spent: 0 }
                })
                .collect(),
            board: Board::restore(records.iter().map(|(_, record)| record)),
            member_steps,
            budget,
            reserved: carried,
            checks: 0,
            carried,
            deadline: Instant::now() + Duration::from_millis(left_ms),
            deadline_at_ms: now_ms() + left_ms,
            phase: Phase::Running,
            queue: VecDeque::new(),
            checking: None,
            reporting: Vec::new(),
            usage: (carried, None),
            next_seq: next_seq_of(&records_path(&self.options.state, project, session, id)),
            unwritten: 0,
        })
    }

    pub(super) fn resume(
        &mut self,
        kernel: &mut Kernel,
        project: &str,
        session: &str,
        payload: &Payload,
    ) -> Result<Result<Payload, Error>, ChannelError> {
        let request = match payload.decode::<ResumeRequest>().and_then(|r| r.validate().map(|()| r)) {
            Ok(request) => request,
            Err(e) => return Ok(Err(e)),
        };
        let found = match &request.swarm {
            Some(id) => self.interrupted_records(project, session, id).map(|records| (id.clone(), records)),
            None => self.only_interrupted(project, session),
        };
        let (id, records) = match found {
            Ok(found) => found,
            Err(e) => return Ok(Err(e)),
        };
        let dir = records.iter().find_map(|(_, record)| match record {
            SwarmRecord::Started { dir, .. } => Some(dir.as_str()),
            _ => None,
        });
        if let Some(dir) = dir.filter(|dir| *dir != format!(".ultramath/{id}")) {
            return Ok(Err(invalid(format!(
                "swarm {id} keeps its files in {dir}, the layout before .ultramath/, where a resumed member \
                 would not find its transcript: it cannot be resumed. ultramath_records with swarm {id} shows \
                 what it did"
            ))));
        }
        let left_ms = match request.minutes {
            Some(minutes) => u64::from(minutes) * 60_000,
            None => match deadline_at_ms(&records).saturating_sub(now_ms()) {
                left if left >= LEAST_LEFT_MS => left,
                _ => {
                    return Ok(Err(invalid(format!(
                        "the time of swarm {id} is over, or less than two minutes of it are left: name how many \
                         minutes it may run from now with minutes"
                    ))));
                }
            },
        };
        let mut swarm = match self.rebuild(project, session, &id, &records, left_ms) {
            Ok(swarm) => swarm,
            Err(e) => return Ok(Err(e)),
        };
        let working: Vec<String> = swarm
            .members
            .iter()
            .filter(|m| m.state == MemberState::Running)
            .map(|m| m.name.clone())
            .collect();
        let waiting: Vec<String> = swarm.board.pending().map(|s| s.member.clone()).collect();
        if working.is_empty() && waiting.is_empty() {
            return Ok(Err(invalid(format!(
                "nothing of swarm {id} can go on: no member was working when it stopped, and no report waits"
            ))));
        }
        if let Err(e) = self.remember(&swarm) {
            return Ok(Err(failed(format!("the swarm's state record: {e}"))));
        }
        // Until each starts again, the members that were working run no process.
        for member in swarm.members.iter_mut().filter(|m| m.state == MemberState::Running) {
            member.state = MemberState::Ended;
        }
        // The members it is to start again; the member records that follow say which did.
        let state = self.options.state.clone();
        swarm.write(
            &state,
            &SwarmRecord::Resumed {
                swarm: id.clone(),
                deadline_at_ms: swarm.deadline_at_ms,
                calls: swarm.carried,
                members: working.clone(),
            },
        );
        let (deadline_at_ms, carried, budget) = (swarm.deadline_at_ms, swarm.carried, swarm.budget);
        self.latest.insert((project.to_owned(), session.to_owned()), id.clone());
        // Held before its members start, so that a failure on the way leaves it known.
        self.swarms.insert(id.clone(), swarm);
        let mut started = Vec::new();
        for name in &working {
            let Some(mut swarm) = self.swarms.remove(&id) else { break };
            let holding = swarm.board.held_by(name).map(|d| d.id.clone());
            let task = members::resume_task(
                holding.as_deref(),
                swarm.member_steps,
                swarm.minutes_left(),
                &swarm.task,
            );
            let run = self.run_member(kernel, &mut swarm, name, &task, holding.is_some(), true);
            self.swarms.insert(id.clone(), swarm);
            started.push((name.clone(), run?));
        }
        let mut restarted = Vec::new();
        for (name, run) in started {
            match run {
                Ok(()) => {
                    let process =
                        self.swarms.get(&id).and_then(|s| s.members.iter().find(|m| m.name == name)?.process);
                    let record = SwarmRecord::Member {
                        swarm: id.clone(),
                        member: name.clone(),
                        state: MemberState::Running,
                        note: "resumed on its transcript".into(),
                        process,
                    };
                    self.record(&id, record);
                    restarted.push(name);
                }
                // It cannot go on: what it held is given up, with why.
                Err(e) => self.member_ended(&id, &name, &format!("it could not be resumed: {}", e.message)),
            }
        }
        // With no member started again and no report waiting, it ends here.
        self.settle(kernel, &id, false)?;
        let ended = match self.swarms.get(&id).map(|swarm| swarm.phase) {
            Some(Phase::Ended(outcome)) => Some(outcome),
            _ => None,
        };
        Ok(Payload::encode(&ResumeResponse {
            swarm: id,
            restarted,
            waiting,
            ended,
            calls: carried,
            budget,
            deadline_at_ms,
        }))
    }
}
