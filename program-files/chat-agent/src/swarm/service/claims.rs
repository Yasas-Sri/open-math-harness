//! A member's calls on its own slot: the board, a
//! registration and its duplicate check, giving a direction up, findings and
//! a report, which reaches the session's agent as a reminder before the
//! submission is taken.

use std::time::{Duration, Instant};

use nucleus_model_interface as model;
use nucleus_protocol::{Error, ErrorCode, Payload, RequestId};
use nucleus_sdk::{ChannelError, Dispatch};
use nucleus_swarm_interface::{
    self as wire, AbandonRequest, BoardRequest, MemberState, PostRequest, PostResponse, ProgressRequest,
    RegisterRequest, RegisterResponse, SubmitRequest, SwarmRecord,
};
use serde_json::json;

use super::{
    CHECK_DEADLINE_MS, Call, Kernel, MAX_QUEUED, Phase, REPLY_MARGIN, Registration, Report, Service, Swarm,
    failed, invalid,
};
use crate::swarm::board::preview;
use crate::swarm::check;

impl Service {
    /// A member's call on its slot `[project, session, swarm, member]`.
    /// `None` when a registration waits for its duplicate check.
    pub(super) fn member_call(
        &mut self,
        kernel: &mut Kernel,
        d: &Dispatch,
    ) -> Result<Option<Result<Payload, Error>>, ChannelError> {
        let segments = d.resource.segments();
        let (project, session, id, name) = (&segments[0], &segments[1], &segments[2], &segments[3]);
        let Some(swarm) = self.swarms.get_mut(id.as_str()) else {
            return Ok(Some(Err(invalid(format!("no swarm {id}")))));
        };
        if swarm.project != *project || swarm.session != *session {
            return Ok(Some(Err(invalid(format!("no swarm {id} in this session")))));
        }
        // The slot is the process the service started for it, nothing else.
        if !swarm.members.iter().any(|m| m.name == *name && m.process == Some(d.caller)) {
            return Ok(Some(Err(Error::new(
                ErrorCode::PermissionDenied,
                "this slot belongs to another member's current run",
            ))));
        }
        let action = d.action.as_str();
        if swarm.phase != Phase::Running && ![wire::ACTION_BOARD, wire::ACTION_PROGRESS].contains(&action) {
            return Ok(Some(Err(invalid("the swarm no longer takes work"))));
        }
        let swarm_id = swarm.id.clone();
        let member = name.clone();
        match action {
            wire::ACTION_BOARD => Ok(Some(member_board(swarm, &member, &d.payload))),
            // What the member's loop counts of its own run, never more than the run may make.
            wire::ACTION_PROGRESS => {
                let request = match d.payload.decode::<ProgressRequest>() {
                    Ok(request) => request,
                    Err(e) => return Ok(Some(Err(e))),
                };
                let steps = u64::from(swarm.member_steps);
                if let Some(m) = swarm.members.iter_mut().find(|m| m.name == member) {
                    m.calls = m.calls.max(request.calls.min(steps));
                }
                Ok(Some(Payload::encode(&json!({}))))
            }
            wire::ACTION_REGISTER => {
                let request =
                    match d.payload.decode::<RegisterRequest>().and_then(|r| r.validate().map(|()| r)) {
                        Ok(request) => request,
                        Err(e) => return Ok(Some(Err(e))),
                    };
                if let Err(e) = swarm.board.may_register(&member) {
                    return Ok(Some(Err(e)));
                }
                if let Some(same) = swarm.board.same_text(&request.direction) {
                    let (of, text) = (same.id.clone(), same.text.clone());
                    let why = format!("the same text as direction {of}");
                    let refusal = duplicate(&of, &text, &why);
                    self.record(
                        &swarm_id,
                        SwarmRecord::Refused {
                            swarm: swarm_id.clone(),
                            member,
                            text: request.direction,
                            duplicate_of: Some(of),
                            reason: why,
                        },
                    );
                    return Ok(Some(Err(refusal)));
                }
                if swarm.board.directions().is_empty() {
                    let registered = swarm.board.register(&member, &request.direction);
                    return Ok(Some(self.registered(&swarm_id, &member, &request.direction, registered)));
                }
                if swarm.queue.len() >= MAX_QUEUED {
                    return Ok(Some(Err(failed("too many registrations wait; try again in a minute"))));
                }
                let until = Instant::now() + Duration::from_millis(d.remaining_ms);
                swarm.queue.push_back(Registration {
                    dispatch: d.id,
                    member,
                    text: request.direction,
                    until,
                    cancelled: false,
                });
                self.next_check(kernel, &swarm_id)?;
                Ok(None)
            }
            wire::ACTION_ABANDON => {
                let request =
                    match d.payload.decode::<AbandonRequest>().and_then(|r| r.validate().map(|()| r)) {
                        Ok(request) => request,
                        Err(e) => return Ok(Some(Err(e))),
                    };
                let abandoned = swarm.board.abandon(&member, &request.reason);
                Ok(Some(abandoned.and_then(|direction| {
                    self.direction_changed(&swarm_id, &direction);
                    Payload::encode(&json!({}))
                })))
            }
            wire::ACTION_POST => {
                let request = match d.payload.decode::<PostRequest>().and_then(|r| r.validate().map(|()| r)) {
                    Ok(request) => request,
                    Err(e) => return Ok(Some(Err(e))),
                };
                let posted = swarm.board.post(&member, &request);
                Ok(Some(posted.and_then(|(finding, direction)| {
                    self.record(
                        &swarm_id,
                        SwarmRecord::Finding {
                            swarm: swarm_id.clone(),
                            member,
                            direction,
                            finding: finding.clone(),
                            label: request.kind,
                            text: request.text,
                            paths: request.paths,
                        },
                    );
                    Payload::encode(&PostResponse { finding })
                })))
            }
            wire::ACTION_SUBMIT => {
                let request = match d.payload.decode::<SubmitRequest>().and_then(|r| r.validate().map(|()| r))
                {
                    Ok(request) => request,
                    Err(e) => return Ok(Some(Err(e))),
                };
                if swarm.reporting.contains(&member) {
                    return Ok(Some(Err(invalid(
                        "your report is on its way to the agent; wait for its answer",
                    ))));
                }
                let direction = match swarm.board.may_submit(&member) {
                    Ok(direction) => direction,
                    Err(e) => return Ok(Some(Err(e))),
                };
                let session = match swarm.session_resource() {
                    Ok(session) => session,
                    Err(e) => return Ok(Some(Err(e))),
                };
                let text = report_text(&swarm_id, &member, &direction, &request);
                let report = Report {
                    swarm: swarm_id,
                    member: member.clone(),
                    direction,
                    dispatch: d.id,
                    request,
                    cancelled: false,
                };
                match self.wake(kernel, &session, text, Call::Report(Box::new(report)))? {
                    Ok(()) => {
                        if let Some(swarm) = self.swarms.get_mut(id.as_str()) {
                            swarm.reporting.push(member);
                        }
                        Ok(None)
                    }
                    Err(e) => Ok(Some(Err(not_delivered(&e)))),
                }
            }
            _ => Ok(Some(Err(invalid("unknown action")))),
        }
    }

    /// The wake-up carrying a member's report is set, or was refused: the
    /// submission is taken, or refused with why.
    pub(super) fn reported(
        &mut self,
        kernel: &mut Kernel,
        report: Report,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        let Report { swarm: swarm_id, member, direction, dispatch, request, cancelled } = report;
        let outcome = (|| {
            let swarm = self.swarms.get_mut(&swarm_id).ok_or_else(|| invalid("the swarm is gone"))?;
            swarm.reporting.retain(|m| *m != member);
            if let Err(e) = result {
                return Err(not_delivered(&e));
            }
            if swarm.phase != Phase::Running {
                return Err(invalid("the swarm ended while your report was on its way"));
            }
            swarm.board.submit(&member, &direction, &request)?;
            if let Some(m) = swarm.members.iter_mut().find(|m| m.name == member) {
                m.state = MemberState::Submitted;
            }
            Ok(direction)
        })();
        if let Ok(direction) = &outcome {
            let submission = SwarmRecord::Submission {
                swarm: swarm_id.clone(),
                member: member.clone(),
                direction: direction.clone(),
                report: request.report,
                paths: request.paths,
            };
            self.record(&swarm_id, submission);
            let (swarm, state, note, process) =
                (swarm_id.clone(), MemberState::Submitted, String::new(), None);
            self.record(&swarm_id, SwarmRecord::Member { swarm, member, state, note, process });
        }
        if cancelled {
            return Ok(());
        }
        let answer = outcome
            .and_then(|direction| Payload::encode(&json!({"submitted": true, "direction": direction})));
        kernel.reply(dispatch, answer)
    }

    /// Records a direction's new state.
    pub(super) fn direction_changed(&mut self, swarm_id: &str, direction: &str) {
        let Some(swarm) = self.swarms.get(swarm_id) else { return };
        let Some(found) = swarm.board.direction(direction) else {
            return;
        };
        let record = SwarmRecord::Direction {
            swarm: swarm.id.clone(),
            member: found.member.clone(),
            direction: found.id.clone(),
            state: found.state,
            text: found.text.clone(),
            reason: found.reason.clone(),
        };
        self.record(swarm_id, record);
    }

    pub(super) fn registered(
        &mut self,
        swarm_id: &str,
        member: &str,
        text: &str,
        registered: Result<String, Error>,
    ) -> Result<Payload, Error> {
        match registered {
            Ok(direction) => {
                self.direction_changed(swarm_id, &direction);
                Payload::encode(&RegisterResponse { direction })
            }
            Err(e) => {
                self.record(
                    swarm_id,
                    SwarmRecord::Refused {
                        swarm: swarm_id.to_owned(),
                        member: member.to_owned(),
                        text: text.to_owned(),
                        duplicate_of: None,
                        reason: preview(&e.message, 600),
                    },
                );
                Err(e)
            }
        }
    }

    /// Starts the duplicate check of the swarm's next waiting registration,
    /// unless one runs.
    pub(super) fn next_check(&mut self, kernel: &mut Kernel, swarm_id: &str) -> Result<(), ChannelError> {
        loop {
            let Some(swarm) = self.swarms.get_mut(swarm_id) else { return Ok(()) };
            if swarm.checking.is_some() {
                return Ok(());
            }
            let Some(registration) = swarm.queue.pop_front() else { return Ok(()) };
            let left =
                registration.until.saturating_duration_since(Instant::now()).saturating_sub(REPLY_MARGIN);
            let deadline = u64::try_from(left.as_millis()).unwrap_or(u64::MAX).min(CHECK_DEADLINE_MS);
            let refusal = if swarm.phase != Phase::Running {
                Some(invalid("the swarm no longer takes work"))
            } else if let Err(e) = swarm.board.may_register(&registration.member) {
                Some(e)
            } else if swarm.reserved + 1 > swarm.budget {
                Some(invalid("the swarm's model budget is spent: no direction can be checked"))
            } else if deadline < 5_000 {
                Some(failed("too little of your call's time was left to check the direction; try again"))
            } else {
                None
            };
            if let Some(refusal) = refusal {
                kernel.reply(registration.dispatch, Err(refusal))?;
                continue;
            }
            let request = check::request(&swarm.board, &registration.text, &swarm.project);
            let submitted = kernel.submit(
                swarm.model_handle,
                model::ACTION_COMPLETE,
                &swarm.model_resource,
                &request,
                Some(deadline),
            )?;
            match submitted {
                Ok(id) => {
                    swarm.reserved += 1;
                    swarm.checks += 1;
                    swarm.checking = Some((id, registration));
                    self.calls.insert(id, Call::Check(swarm_id.to_owned()));
                    return Ok(());
                }
                Err(e) => {
                    kernel.reply(registration.dispatch, Err(failed(format!("the duplicate check: {e}"))))?;
                }
            }
        }
    }

    pub(super) fn checked(
        &mut self,
        kernel: &mut Kernel,
        swarm_id: &str,
        request: RequestId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        let Some(swarm) = self.swarms.get_mut(swarm_id) else { return Ok(()) };
        let Some((id, registration)) = swarm.checking.take() else { return Ok(()) };
        if id != request {
            swarm.checking = Some((id, registration));
            return Ok(());
        }
        // The caller gave up meanwhile: nothing is registered for it.
        if registration.cancelled || Instant::now() >= registration.until {
            return self.next_check(kernel, swarm_id);
        }
        if swarm.phase != Phase::Running {
            kernel.reply(registration.dispatch, Err(invalid("the swarm no longer takes work")))?;
            return self.next_check(kernel, swarm_id);
        }
        let decided = result
            .and_then(|payload| payload.decode::<model::CompletionResponse>())
            .map_err(|e| e.message)
            .and_then(|response| check::decision(&response, &swarm.board));
        let outcome = match decided {
            Ok(check::Decision::Distinct) => {
                let registered = swarm.board.register(&registration.member, &registration.text);
                self.registered(swarm_id, &registration.member, &registration.text, registered)
            }
            Ok(check::Decision::Duplicate { of, reason }) => {
                let text = swarm.board.direction(&of).map(|d| d.text.clone()).unwrap_or_default();
                let refusal = duplicate(&of, &text, &reason);
                self.record(
                    swarm_id,
                    SwarmRecord::Refused {
                        swarm: swarm_id.to_owned(),
                        member: registration.member.clone(),
                        text: registration.text.clone(),
                        duplicate_of: Some(of),
                        reason,
                    },
                );
                Err(refusal)
            }
            Err(why) => {
                Err(failed(format!("the duplicate check failed ({why}); nothing was registered: try again")))
            }
        };
        kernel.reply(registration.dispatch, outcome)?;
        self.next_check(kernel, swarm_id)
    }

    /// `name`'s run ended without a submission waiting: what it held is
    /// given up, so that nobody repeats it.
    pub(super) fn member_ended(&mut self, swarm_id: &str, name: &str, why: &str) {
        let Some(swarm) = self.swarms.get_mut(swarm_id) else { return };
        let given_up = swarm.board.member_ended(name, why);
        let record = SwarmRecord::Member {
            swarm: swarm_id.to_owned(),
            member: name.to_owned(),
            state: MemberState::Ended,
            note: why.to_owned(),
            process: None,
        };
        if let Some(direction) = given_up {
            self.direction_changed(swarm_id, &direction);
        }
        self.record(swarm_id, record);
    }
}

/// What wakes the session's agent with a member's report: the member's
/// words, framed as theirs, and the files it names.
pub(super) fn report_text(swarm: &str, member: &str, direction: &str, request: &SubmitRequest) -> String {
    let mut text = format!(
        "Swarm {swarm}: member {member} submitted a solution under direction {direction}. Its report, in its \
         model's words:\n{}\n",
        request.report.trim_end()
    );
    if !request.paths.is_empty() {
        text.push_str(&format!(
            "The files it names, relative to the project's repository: {}\n",
            request.paths.join(", ")
        ));
    }
    text.push_str(
        "Check them and give your verdict with ultramath_verdict; ultramath_status and ultramath_records show the board \
         and the swarm's records.",
    );
    text
}

/// The refusal of a submission whose report could not reach the agent.
fn not_delivered(why: &Error) -> Error {
    Error::new(
        why.code,
        format!(
            "your report could not reach the agent that started the swarm ({}); nothing was submitted: \
             submit it again later",
            why.message
        ),
    )
}

/// The refusal of a duplicate registration, naming the direction it repeats.
pub(super) fn duplicate(of: &str, text: &str, why: &str) -> Error {
    invalid(format!(
        "refused: the same direction as {of} (\"{}\"): {why}. Read the board and choose a genuinely different direction.",
        preview(text, 300)
    ))
}

/// What a member reads with `board`: its own claim, and the board or a page.
pub(super) fn member_board(swarm: &Swarm, member: &str, payload: &Payload) -> Result<Payload, Error> {
    let request = payload.decode::<BoardRequest>()?;
    let you = json!({"member": member, "direction": swarm.board.held_by(member).map(|d| d.id.clone())});
    match request.direction {
        Some(direction) => Payload::encode(&json!({"you": you, "direction": swarm.board.page(&direction)?})),
        None => Payload::encode(&json!({"you": you, "board": swarm.board.overview()})),
    }
}
