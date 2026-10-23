//! Starting a swarm: the members' directories made by the
//! project's shell in the project's repository, where the session works, the
//! state record, then every member's first run, and the swarm's first records.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use nucleus_projects_interface::Dir;
use nucleus_protocol::{Error, IdentityRequest, Payload};
use nucleus_sdk::{ChannelError, Dispatch};
use nucleus_shell_interface::{ExecRequest, ExecResponse};
use nucleus_swarm_interface::{MemberState, StartRequest, StartResponse, SwarmRecord};

use super::{
    Call, Kernel, MAX_ENDED, Member, PREPARE_DEADLINE_MS, Phase, Service, Starting, Swarm, base36, failed,
    invalid, now_ms,
};
use crate::swarm::board::{Board, preview};
use crate::swarm::members;

impl Service {
    pub(super) fn start(
        &mut self,
        kernel: &mut Kernel,
        d: &Dispatch,
    ) -> Result<Result<(), Error>, ChannelError> {
        let (project, session) = (d.resource.segments()[0].clone(), d.resource.segments()[1].clone());
        let start = match d.payload.decode::<StartRequest>().and_then(StartRequest::validate) {
            Ok(start) => start,
            Err(e) => return Ok(Err(e)),
        };
        let wanted = start.model.clone().unwrap_or_else(|| self.options.models[0].clone());
        let Some(model) = self.authority.models.iter().find(|(id, _, _)| *id == wanted).cloned() else {
            let offered: Vec<&str> = self.authority.models.iter().map(|(id, _, _)| id.as_str()).collect();
            return Ok(Err(invalid(format!(
                "model {wanted} is not offered to swarms; these are: {offered:?}"
            ))));
        };
        let ms = now_ms().max(self.last_id_ms + 1);
        self.last_id_ms = ms;
        let id = format!("s{}", base36(ms));
        // Every session works in its project's repository.
        let root = match Dir::repository(&project) {
            Ok(dir) => dir.resource(),
            Err(e) => return Ok(Err(e)),
        };
        let mut command = format!(
            "mkdir -p .ultramath\n[ -e .ultramath/.gitignore ] || printf '*\\n' > .ultramath/.gitignore\nmkdir .ultramath/{id}\n\
             mkdir .ultramath/{id}/transcripts"
        );
        for n in 1..=start.members {
            command.push_str(&format!(" .ultramath/{id}/m{n}"));
        }
        let exec = ExecRequest {
            command,
            stdin: None,
            timeout_ms: Some(PREPARE_DEADLINE_MS - 10_000),
            max_output_bytes: Some(2048),
            ..Default::default()
        };
        let submitted = kernel.submit(
            self.authority.held.shell,
            nucleus_shell_interface::ACTION_EXEC,
            &root,
            &exec,
            Some(PREPARE_DEADLINE_MS),
        )?;
        let request = match submitted {
            Ok(request) => request,
            Err(e) => return Ok(Err(e)),
        };
        let key = self.next_start;
        self.next_start += 1;
        self.calls.insert(request, Call::Prepare(key));
        self.starting.insert(
            key,
            Starting { dispatch: d.id, project, session, start, model, id, root, cancelled: false },
        );
        Ok(Ok(()))
    }

    /// Refuses a start that cannot go on, leaving nothing behind.
    pub(super) fn start_failed(
        &mut self,
        kernel: &mut Kernel,
        key: u64,
        why: Error,
    ) -> Result<(), ChannelError> {
        if let Some(start) = self.starting.remove(&key)
            && !start.cancelled
        {
            kernel.reply(start.dispatch, Err(why))?;
        }
        Ok(())
    }

    pub(super) fn prepared(
        &mut self,
        kernel: &mut Kernel,
        key: u64,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        let ran = result.and_then(|payload| payload.decode::<ExecResponse>());
        let Some(start) = self.starting.get(&key) else { return Ok(()) };
        if start.cancelled {
            self.starting.remove(&key);
            return Ok(());
        }
        match ran {
            Ok(done) if done.exit_code == Some(0) => {}
            Ok(done) => {
                let why = failed(format!(
                    "could not make .ultramath/{} in the session's working directory: {}",
                    start.id,
                    preview(done.output.trim(), 600)
                ));
                return self.start_failed(kernel, key, why);
            }
            Err(e) => return self.start_failed(kernel, key, e),
        }
        let Some(start) = self.starting.remove(&key) else { return Ok(()) };
        let root = start.root.clone();
        let (model, model_resource, model_handle) = start.model.clone();
        let budget = 2 * u64::from(start.start.members) * u64::from(start.start.member_steps);
        let minutes = u64::from(start.start.minutes);
        let mut swarm = Swarm {
            id: start.id.clone(),
            project: start.project.clone(),
            session: start.session.clone(),
            root,
            task: start.start.task.clone(),
            model,
            model_resource,
            model_handle,
            effort: start.start.reasoning_effort,
            members: (1..=start.start.members)
                .map(|n| Member {
                    name: format!("m{n}"),
                    process: None,
                    state: MemberState::Ended,
                    runs: 0,
                    calls: 0,
                    spent: 0,
                })
                .collect(),
            board: Board::default(),
            member_steps: start.start.member_steps,
            budget,
            reserved: 0,
            checks: 0,
            carried: 0,
            deadline: Instant::now() + Duration::from_secs(minutes * 60),
            deadline_at_ms: now_ms() + minutes * 60_000,
            phase: Phase::Running,
            queue: VecDeque::new(),
            checking: None,
            reporting: Vec::new(),
            usage: (0, None),
            next_seq: 0,
            unwritten: 0,
        };
        if let Err(e) = self.remember(&swarm) {
            return kernel.reply(start.dispatch, Err(failed(format!("the swarm's state record: {e}"))));
        }
        let names: Vec<String> = swarm.members.iter().map(|m| m.name.clone()).collect();
        for name in &names {
            let task = members::first_task(
                &swarm.slot(name),
                names.len(),
                swarm.member_steps,
                swarm.minutes_left(),
                &swarm.task,
            );
            if let Err(e) = self.run_member(kernel, &mut swarm, name, &task, false, false)? {
                // Half a swarm is no swarm: stop what started and say why.
                for member in swarm.members.iter_mut().filter(|m| m.process.is_some()) {
                    if let Some(process) = member.process.take() {
                        let _ = kernel.kill(process)?;
                    }
                }
                self.unmark(&swarm.id);
                return kernel.reply(start.dispatch, Err(e));
            }
        }
        let response = StartResponse {
            swarm: swarm.id.clone(),
            dir: format!(".ultramath/{}", swarm.id),
            members: names.clone(),
            model: swarm.model.clone(),
            reasoning_effort: swarm.effort,
            member_steps: swarm.member_steps,
            budget: swarm.budget,
            deadline_at_ms: swarm.deadline_at_ms,
        };
        let state = self.options.state.clone();
        swarm.write(
            &state,
            &SwarmRecord::Started {
                swarm: swarm.id.clone(),
                project: swarm.project.clone(),
                session: swarm.session.clone(),
                task: swarm.task.clone(),
                dir: response.dir.clone(),
                members: names.clone(),
                model: swarm.model.clone(),
                reasoning_effort: swarm.effort,
                member_steps: swarm.member_steps,
                budget: swarm.budget,
                deadline_at_ms: swarm.deadline_at_ms,
            },
        );
        let running: Vec<SwarmRecord> = swarm
            .members
            .iter()
            .map(|member| SwarmRecord::Member {
                swarm: swarm.id.clone(),
                member: member.name.clone(),
                state: MemberState::Running,
                note: String::new(),
                process: member.process,
            })
            .collect();
        for record in &running {
            swarm.write(&state, record);
        }
        self.latest.insert((swarm.project.clone(), swarm.session.clone()), swarm.id.clone());
        self.swarms.insert(swarm.id.clone(), swarm);
        self.prune();
        kernel.reply(start.dispatch, Payload::encode(&response))
    }

    /// Keeps of the swarms that ended only each session's latest, and at
    /// most [`MAX_ENDED`] of those, the newest: what `status` can still show.
    pub(super) fn prune(&mut self) {
        let latest: std::collections::BTreeSet<&String> = self.latest.values().collect();
        let mut ended: Vec<String> = self
            .swarms
            .values()
            .filter(|s| !s.running())
            .map(|s| s.id.clone())
            .filter(|id| latest.contains(id))
            .collect();
        let gone: Vec<String> = self
            .swarms
            .values()
            .filter(|s| !s.running() && !latest.contains(&s.id))
            .map(|s| s.id.clone())
            .collect();
        // Ids grow with their start time, so the oldest sort first.
        ended.sort_by_key(|id| (id.len(), id.clone()));
        let surplus = ended.len().saturating_sub(MAX_ENDED);
        for id in gone.into_iter().chain(ended.into_iter().take(surplus)) {
            self.swarms.remove(&id);
        }
        let swarms = &self.swarms;
        self.latest.retain(|_, id| swarms.contains_key(id));
    }

    /// Starts one run of `name`: fresh, or continuing its transcript.
    pub(super) fn run_member(
        &mut self,
        kernel: &mut Kernel,
        swarm: &mut Swarm,
        name: &str,
        task: &str,
        holding: bool,
        resuming: bool,
    ) -> Result<Result<(), Error>, ChannelError> {
        if swarm.phase != Phase::Running {
            return Ok(Err(invalid("the swarm no longer starts members")));
        }
        let left = swarm.deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_secs(60) {
            return Ok(Err(invalid("less than a minute of the swarm's time is left")));
        }
        let steps = swarm.member_steps;
        if swarm.reserved + u64::from(steps) > swarm.budget {
            return Ok(Err(invalid("the swarm's model budget is spent")));
        }
        let slot = swarm.slot(name);
        let web = self.authority.held.web.as_ref().map(|(_, _, resource)| resource);
        let prepared = (|| {
            let ttl = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
            let grants = members::grants(
                &self.authority.held,
                &slot,
                (swarm.model_handle, &swarm.model_resource),
                steps,
                ttl,
            )?;
            let bindings = members::bindings(&slot, web)?;
            let run = members::Run {
                executable: &self.options.member_executable,
                args: &self.options.member_args,
                model: &swarm.model,
                effort: swarm.effort,
                steps,
                task,
                holding,
                resuming,
                as_project: self.options.projects.is_some() && self.authority.spawn_as.is_some(),
            };
            Ok::<_, Error>((grants, members::program(&run, &slot, &bindings)?))
        })();
        let (grants, program) = match prepared {
            Ok(prepared) => prepared,
            Err(e) => return Ok(Err(e)),
        };
        let identity = self
            .options
            .projects
            .as_ref()
            .zip(self.authority.spawn_as)
            .map(|(_, authority)| IdentityRequest { authority, project: swarm.project.clone() });
        let child = match kernel.spawn_executable_as(self.authority.spawn, program, grants, identity)? {
            Ok(child) => child,
            Err(e) => return Ok(Err(e)),
        };
        swarm.reserved += u64::from(steps);
        if let Some(member) = swarm.members.iter_mut().find(|m| m.name == name) {
            member.process = Some(child);
            member.state = MemberState::Running;
            member.runs += 1;
            member.spent += std::mem::take(&mut member.calls);
        }
        Ok(Ok(()))
    }
}
