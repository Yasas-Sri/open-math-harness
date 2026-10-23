//! The process of one discovery run: started by the service as
//! `nucleus-discovery run …` ([`RunCommand`]) with its run's narrowing of the
//! service's authority. It provides its own endpoint and says so in its first
//! report, admits the run when the service asks (a new run's statements
//! registered and its directories created, or a saved run's checkpoint closed
//! and loaded), waits for `begin`, and then runs the run to its end in the
//! harness. It serves nobody but the service.

use super::*;
use crate::harness::{self, Admitted, Harness, Kernel};
use crate::progress::Progress;
use crate::wire::{self, Admission, RunCommand};
use crate::workers::Authority;

/// The `lean` calls of an admission, on the process's own capabilities.
struct Lean<'k> {
    kernel: &'k mut Kernel,
    authority: &'k Authority,
}

impl admit::LeanService for Lean<'_> {
    fn call(
        &mut self,
        action: &str,
        resource: &ResourcePath,
        request: &Payload,
        deadline_ms: u64,
    ) -> Result<Result<Payload, Error>, ChannelError> {
        harness::lean_call(self.kernel, self.authority, action, resource, request, deadline_ms)
    }
}

/// Serves the run's endpoint until the run ends, its admission is refused,
/// or the kernel terminates this process.
pub fn serve(command: &RunCommand) -> Result<(), String> {
    let options = &command.options;
    options.check_paths()?;
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = Authority::from(
        kernel.handles(),
        options,
        &command.run,
        &command.endpoint,
        &command.report_endpoint,
    )?;
    let declared = wire::run_interface(command.endpoint.clone()).map_err(|e| e.message)?;
    kernel
        .provide(authority.provide, declared)
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot provide `{}`: {:?}: {}", command.endpoint, e.code, e.message))?;
    harness::send_report(&mut kernel, &authority, &wire::Report::ready())
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("the service refused the first report: {:?}: {}", e.code, e.message))?;
    match admission(&mut kernel, &authority, command) {
        Ok(Some(admitted)) => Harness::new(kernel, authority, options, admitted).serve(),
        Ok(None) | Err(ChannelError::Terminated) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// The process was started by a service of another protocol version: it
/// tells the service so in the one form every version reads, a report that
/// carries only its own `v`, and exits (`wire`, "Versions").
pub fn refuse_protocol(service: u32) -> Result<(), String> {
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let report = kernel
        .handles()
        .iter()
        .find(|h| h.actions.iter().any(|a| a.as_str() == wire::ACTION_REPORT))
        .map(|h| (h.handle, h.resource.clone()));
    if let Some((handle, resource)) = report {
        let only_the_version = serde_json::json!({ "v": wire::PROTOCOL });
        let _ = kernel.call(handle, wire::ACTION_REPORT, &resource, &only_the_version, Some(30_000));
    }
    Err(wire::mismatch(service).message)
}

/// Waits for the service's `admit` and admits the run; `None` when it is
/// refused (the service got the refusal and kills this process) or the
/// process is terminated first. Nothing else is served before it.
fn admission(
    kernel: &mut Kernel,
    authority: &Authority,
    command: &RunCommand,
) -> Result<Option<Admitted>, ChannelError> {
    let tool_loop = authority.shell.is_some();
    loop {
        let d = match kernel.next_event()? {
            Event::Dispatch(d) => d,
            Event::Terminate => return Ok(None),
            Event::ChildExited(_) | Event::ChildReaped { .. } | Event::CancelDispatch(_) => continue,
        };
        if d.action.as_str() != wire::ACTION_ADMIT {
            kernel.reply(d.id, Err(invalid(format!("run {} is not admitted", command.run))))?;
            continue;
        }
        // The time the call has left, counted from when it was taken up
        // (the service gave the call its own caller's time).
        let taken = Instant::now();
        let outcome = match wire::decode::<wire::Admit>(&d.payload) {
            Err(e) => Err(e),
            Ok(admit) => {
                let until = admit::until(taken, None, d.remaining_ms);
                let mut lean = Lean { kernel: &mut *kernel, authority };
                match admit.admission {
                    Admission::Start { task } => {
                        start(&mut lean, command, &authority.key, *task, until, tool_loop)?
                    }
                    Admission::Resume => resume::admit(&mut lean, command, &authority.key, until, tool_loop)?,
                }
            }
        };
        return match outcome {
            Ok(admitted) => {
                let answer = wire::encode(&wire::Admitted {
                    v: wire::PROTOCOL,
                    status: admitted.progress.status(Spend::default()),
                    uncertain_model_calls: admitted.uncertain_model_calls,
                });
                let accepted = answer.is_ok();
                kernel.reply(d.id, answer)?;
                Ok(accepted.then_some(admitted))
            }
            Err(e) => {
                kernel.reply(d.id, Err(e))?;
                Ok(None)
            }
        };
    }
}

/// `discovery start` of `task`, which the service accepted and normalized:
/// resolved again under the same options, its problems and statements
/// admitted by Lean by `until` ([`admit::create`], which then creates the
/// run's directories), and the run's lease taken.
fn start(
    lean: &mut dyn admit::LeanService,
    command: &RunCommand,
    key: &str,
    task: Task,
    until: Instant,
    tool_loop: bool,
) -> Result<Result<Admitted, Error>, ChannelError> {
    if task.name != command.run {
        return Ok(Err(invalid(format!("the task names run {:?}, not {:?}", task.name, command.run))));
    }
    let plan = match resolve_task(&task, &command.options, &command.options.out)
        .and_then(|plan| resume::fits_tool_loop(&plan.prompt_template, tool_loop).map(|()| plan))
    {
        Ok(plan) => plan,
        Err(e) => return Ok(Err(e)),
    };
    if let Err(e) = admit::create(&plan, key, until, lean, command.endpoint.as_str())? {
        return Ok(Err(e));
    }
    Ok(resume::lease(&plan.dir).map(|lease| Admitted {
        progress: Progress::new(plan),
        saved: BTreeMap::new(),
        next_task: 0,
        lease,
        uncertain_model_calls: 0,
    }))
}
