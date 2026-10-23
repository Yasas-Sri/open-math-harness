//! Observable behavior of the kernel state machine, driven only through its
//! public interface. These tests establish the authorization and lifetime
//! logic; they say nothing about host isolation, which is tested with real
//! child processes elsewhere.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use nucleus_kernel::{CapSpec, Config, Kernel, Namespace, NamespaceBind, Output, SecurityEvent};
use nucleus_protocol::{
    ActionDecl, ActionName, ChildBind, ChildInfo, ChildProgram, DispatchId, EffectClass, EndpointName, Error,
    ErrorCode, FromKernel, Grant, HandleId, InterfaceDecl, Payload, ProcessId, ProgramName, RequestId,
    ResourceFrom, ResourcePath, ResponseBody, ToKernel,
};

fn name(s: &str) -> EndpointName {
    EndpointName::new(s).unwrap()
}
fn action(s: &str) -> ActionName {
    ActionName::new(s).unwrap()
}
fn actions(list: &[&str]) -> BTreeSet<ActionName> {
    list.iter().map(|a| action(a)).collect()
}
fn path(segments: &[&str]) -> ResourcePath {
    ResourcePath::new(segments.iter().copied()).unwrap()
}
fn program(s: &str) -> ProgramName {
    ProgramName::new(s).unwrap()
}
fn payload() -> Payload {
    Payload::encode(&serde_json::json!({})).unwrap()
}
fn spec(endpoint: &str, acts: &[&str], resource: &[&str]) -> CapSpec {
    CapSpec {
        endpoint: name(endpoint),
        actions: actions(acts),
        resource: path(resource),
        ttl_ms: None,
        delegable: true,
        max_calls: None,
    }
}
fn grant(from: u32, acts: &[&str], resource: &[&str]) -> Grant {
    Grant {
        from: HandleId(from),
        actions: actions(acts),
        resource: path(resource),
        ttl_ms: None,
        delegable: false,
        max_calls: None,
    }
}

/// A kernel with an `fs` provider (handle 0: provide), an agent
/// (handle 0: fs read+write on /work, handle 1: spawn worker, handle 2: spawn
/// an executable under /plugins), and helpers.
struct World {
    kernel: Kernel,
    now: u64,
    fs: ProcessId,
    agent: ProcessId,
    next_request: u64,
}

fn config() -> Config {
    let mut config =
        Config::with_programs(["fs", "agent", "worker", "other"].iter().map(|p| program(p)).collect());
    config.max_inflight_per_endpoint = 1;
    config.max_queue_per_endpoint = 2;
    config.max_pending_per_caller = 8;
    // The agent can see /work (writable) and /plugins (read-only), no network.
    config.namespaces.insert(
        program("agent"),
        Namespace {
            binds: vec![
                NamespaceBind { path: "/work".to_owned(), source: "/work".into(), writable: true },
                NamespaceBind { path: "/plugins".to_owned(), source: "/plugins".into(), writable: false },
            ],
            network: false,
        },
    );
    config
}

fn fs_interface() -> InterfaceDecl {
    InterfaceDecl {
        endpoint: name("fs"),
        version: 1,
        description: String::new(),
        actions: BTreeMap::from([
            (
                action("read"),
                ActionDecl::tool(
                    EffectClass::Observation,
                    "read a file",
                    serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                    ResourceFrom::Segments { param: "path".to_owned() },
                ),
            ),
            (action("write"), ActionDecl::internal(EffectClass::ReversibleMutation)),
        ]),
    }
}

impl World {
    fn new() -> Self {
        Self::with_config(config())
    }

    fn with_config(config: Config) -> Self {
        let mut kernel = Kernel::new(config).unwrap();
        let fs = kernel
            .boot_process(program("fs"), vec![spec("kernel", &["provide"], &["endpoint", "fs"])], 0)
            .unwrap();
        kernel.handle_message(
            fs,
            ToKernel::Provide { request: RequestId(1), authority: HandleId(0), interface: fs_interface() },
            0,
        );
        let agent = kernel
            .boot_process(
                program("agent"),
                vec![
                    spec("fs", &["read", "write"], &["work"]),
                    spec("kernel", &["spawn"], &["program", "worker"]),
                    // Handle 2: may start executables anywhere under /plugins.
                    spec("kernel", &["spawn_executable"], &["executable", "plugins"]),
                ],
                0,
            )
            .unwrap();
        kernel.drain_outputs();
        Self { kernel, now: 0, fs, agent, next_request: 100 }
    }

    fn send(&mut self, from: ProcessId, msg: ToKernel) -> Vec<Output> {
        self.kernel.handle_message(from, msg, self.now);
        self.kernel.drain_outputs()
    }

    fn call(
        &mut self,
        from: ProcessId,
        handle: u32,
        act: &str,
        resource: &[&str],
    ) -> (RequestId, Vec<Output>) {
        let request = RequestId(self.next_request);
        self.next_request += 1;
        let outputs = self.send(
            from,
            ToKernel::Call {
                request,
                handle: HandleId(handle),
                action: action(act),
                resource: path(resource),
                payload: payload(),
                deadline_ms: Some(1_000),
            },
        );
        (request, outputs)
    }

    fn spawn_worker(&mut self, grants: Vec<Grant>) -> (Result<ResponseBody, Error>, Vec<Output>) {
        let request = RequestId(self.next_request);
        self.next_request += 1;
        let outputs = self.send(
            self.agent,
            ToKernel::Spawn { request, authority: HandleId(1), program: program("worker"), grants },
        );
        (response(&outputs, self.agent, request).expect("spawn always responds"), outputs)
    }

    fn worker(&mut self, grants: Vec<Grant>) -> ProcessId {
        match self.spawn_worker(grants).0 {
            Ok(ResponseBody::Spawned { child }) => child,
            other => panic!("spawn failed: {other:?}"),
        }
    }
}

fn response(outputs: &[Output], to: ProcessId, request: RequestId) -> Option<Result<ResponseBody, Error>> {
    outputs.iter().find_map(|o| match o {
        Output::Send { to: t, msg: FromKernel::Response { request: r, result } }
            if *t == to && *r == request =>
        {
            Some(result.clone())
        }
        _ => None,
    })
}

fn code(outputs: &[Output], to: ProcessId, request: RequestId) -> Option<ErrorCode> {
    response(outputs, to, request).and_then(|r| r.err()).map(|e| e.code)
}

fn dispatches(outputs: &[Output], to: ProcessId) -> Vec<(DispatchId, ProcessId, String)> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::Send { to: t, msg: FromKernel::Dispatch { dispatch, caller, resource, .. } }
                if *t == to =>
            {
                Some((*dispatch, *caller, resource.to_string()))
            }
            _ => None,
        })
        .collect()
}

fn cancelled_dispatches(outputs: &[Output], to: ProcessId) -> Vec<DispatchId> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::Send { to: t, msg: FromKernel::CancelDispatch { dispatch } } if *t == to => {
                Some(*dispatch)
            }
            _ => None,
        })
        .collect()
}

fn denied(outputs: &[Output], process: ProcessId, reason: ErrorCode) -> bool {
    outputs.iter().any(|o| {
        matches!(o, Output::Event(SecurityEvent::Denied { process: p, reason: r, .. }) if *p == process && *r == reason)
    })
}

fn stopped(outputs: &[Output]) -> Vec<ProcessId> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::StopProcess { process } => Some(*process),
            _ => None,
        })
        .collect()
}

fn reply_ok(world: &mut World, dispatch: DispatchId) -> Vec<Output> {
    let fs = world.fs;
    world.send(fs, ToKernel::Reply { dispatch, result: Ok(payload()) })
}

// ---- identity and handles ----------------------------------------------------

#[test]
fn a_message_from_an_unknown_process_has_no_effect() {
    let mut world = World::new();
    let (_, outputs) = world.call(ProcessId(999), 0, "read", &["work", "a"]);
    assert!(dispatches(&outputs, world.fs).is_empty());
    assert!(outputs.iter().all(|o| !matches!(o, Output::Send { .. })));
    assert!(outputs.iter().any(|o| matches!(o, Output::Event(SecurityEvent::ProtocolViolation { .. }))));
}

#[test]
fn fabricated_handles_are_rejected() {
    let mut world = World::new();
    let agent = world.agent;
    // The agent holds handles 0, 1, 2; these are numbers it does not hold.
    for forged in [3, 7, u32::MAX] {
        let (request, outputs) = world.call(agent, forged, "read", &["work", "a"]);
        assert_eq!(code(&outputs, agent, request), Some(ErrorCode::UnknownHandle));
        assert!(dispatches(&outputs, world.fs).is_empty());
        assert!(denied(&outputs, agent, ErrorCode::UnknownHandle));
    }
}

#[test]
fn a_handle_number_means_only_what_the_sender_holds() {
    let mut world = World::new();
    let worker = world.worker(vec![grant(0, &["read"], &["work", "sub"])]);

    // The worker's handle 0 is its own narrow capability, not the agent's handle 0.
    let (request, outputs) = world.call(worker, 0, "write", &["work", "sub", "f"]);
    assert_eq!(code(&outputs, worker, request), Some(ErrorCode::PermissionDenied));
    // The agent's handle 1 (spawn authority) does not exist for the worker.
    let (request, outputs) = world.call(worker, 1, "read", &["work", "sub", "f"]);
    assert_eq!(code(&outputs, worker, request), Some(ErrorCode::UnknownHandle));
}

// ---- delegation --------------------------------------------------------------

#[test]
fn a_child_receives_narrower_authority_and_is_denied_before_dispatch() {
    let mut world = World::new();
    let (result, outputs) = world.spawn_worker(vec![grant(0, &["read"], &["work", "sub"])]);
    let Ok(ResponseBody::Spawned { child }) = result else { panic!("{result:?}") };
    assert!(outputs.iter().any(|o| matches!(o, Output::StartProcess { process, .. } if *process == child)));
    let hello = outputs.iter().find_map(|o| match o {
        Output::Send { to, msg: FromKernel::Hello { handles, .. } } if *to == child => Some(handles.clone()),
        _ => None,
    });
    let handles = hello.expect("child is told what it holds");
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].actions, actions(&["read"]));
    assert_eq!(handles[0].resource, path(&["work", "sub"]));

    let (_, outputs) = world.call(child, 0, "read", &["work", "sub", "ok.txt"]);
    assert_eq!(dispatches(&outputs, world.fs).len(), 1);

    for (act, resource) in
        [("write", vec!["work", "sub", "f"]), ("read", vec!["work", "secret"]), ("read", vec!["work"])]
    {
        let (request, outputs) = world.call(child, 0, act, &resource);
        assert_eq!(code(&outputs, child, request), Some(ErrorCode::PermissionDenied));
        assert!(dispatches(&outputs, world.fs).is_empty(), "provider must not see a denied call");
        assert!(denied(&outputs, child, ErrorCode::PermissionDenied));
    }
}

#[test]
fn a_widened_grant_rejects_the_whole_spawn() {
    let mut world = World::new();
    let before = world.kernel.process_count();
    for bad in [
        grant(0, &["read", "delete"], &["work"]),
        grant(0, &["read"], &[]),
        grant(1, &["provide"], &["program", "worker"]),
    ] {
        let (result, outputs) = world.spawn_worker(vec![grant(0, &["read"], &["work"]), bad]);
        assert_eq!(result.map_err(|e| e.code), Err(ErrorCode::PermissionDenied));
        assert!(outputs.iter().all(|o| !matches!(o, Output::StartProcess { .. })));
    }
    assert_eq!(world.kernel.process_count(), before);
}

#[test]
fn a_non_delegable_grant_cannot_be_passed_on_and_spawn_needs_authority() {
    let mut world = World::new();
    let worker = world.worker(vec![grant(0, &["read"], &["work"])]);
    // The worker has no spawn capability; its only handle is for `fs`.
    let outputs = world.send(
        worker,
        ToKernel::Spawn {
            request: RequestId(1),
            authority: HandleId(0),
            program: program("worker"),
            grants: vec![],
        },
    );
    assert_eq!(code(&outputs, worker, RequestId(1)), Some(ErrorCode::PermissionDenied));

    // The agent may spawn `worker` but not `other`.
    let agent = world.agent;
    let outputs = world.send(
        agent,
        ToKernel::Spawn {
            request: RequestId(2),
            authority: HandleId(1),
            program: program("other"),
            grants: vec![],
        },
    );
    assert_eq!(code(&outputs, agent, RequestId(2)), Some(ErrorCode::PermissionDenied));
    assert!(outputs.iter().all(|o| !matches!(o, Output::StartProcess { .. })));
}

#[test]
fn closing_a_handle_withdraws_what_was_delegated_from_it() {
    let mut world = World::new();
    let agent = world.agent;
    let worker = world.worker(vec![grant(0, &["read"], &["work"])]);
    world.send(agent, ToKernel::Close { request: RequestId(1), handle: HandleId(0) });
    let (request, outputs) = world.call(worker, 0, "read", &["work", "a"]);
    assert_eq!(code(&outputs, worker, request), Some(ErrorCode::Revoked));
    let (request, outputs) = world.call(agent, 0, "read", &["work", "a"]);
    assert_eq!(code(&outputs, agent, request), Some(ErrorCode::UnknownHandle));
}

#[test]
fn expiry_applies_to_calls_still_queued() {
    let mut world = World::new();
    let fs = world.fs;
    let mut short = grant(0, &["read"], &["work"]);
    short.ttl_ms = Some(50);
    let worker = world.worker(vec![short]);
    let (_, outputs) = world.call(worker, 0, "read", &["work", "one"]);
    let first = dispatches(&outputs, fs)[0].0;
    let (second, _) = world.call(worker, 0, "read", &["work", "two"]);

    world.now = 60;
    let outputs = reply_ok(&mut world, first);
    assert_eq!(code(&outputs, worker, second), Some(ErrorCode::Revoked));
    assert!(dispatches(&outputs, fs).is_empty());
}

#[test]
fn a_budget_is_charged_at_dispatch() {
    let mut config = config();
    config.max_inflight_per_endpoint = 4;
    let mut world = World::with_config(config);
    let fs = world.fs;
    let mut limited = grant(0, &["read"], &["work"]);
    limited.max_calls = Some(2);
    limited.delegable = false;
    let worker = world.worker(vec![limited]);
    for _ in 0..2 {
        let (_, outputs) = world.call(worker, 0, "read", &["work", "a"]);
        assert_eq!(dispatches(&outputs, fs).len(), 1);
    }
    let (request, outputs) = world.call(worker, 0, "read", &["work", "a"]);
    assert_eq!(code(&outputs, worker, request), Some(ErrorCode::BudgetExhausted));
    assert!(dispatches(&outputs, fs).is_empty());
}

// ---- provider failure and replacement ------------------------------------------

#[test]
fn a_provider_crash_fails_its_callers_honestly_and_nobody_else() {
    let mut config = config();
    config.max_inflight_per_endpoint = 2;
    let mut world = World::with_config(config);
    let (agent, fs) = (world.agent, world.fs);

    // An unrelated endpoint with its own caller.
    let echo = world
        .kernel
        .boot_process(program("other"), vec![spec("kernel", &["provide"], &["endpoint", "echo"])], 0)
        .unwrap();
    let echo_interface = InterfaceDecl {
        endpoint: name("echo"),
        version: 1,
        description: String::new(),
        actions: BTreeMap::from([(action("say"), ActionDecl::internal(EffectClass::Observation))]),
    };
    world.send(
        echo,
        ToKernel::Provide { request: RequestId(1), authority: HandleId(0), interface: echo_interface },
    );
    let bystander =
        world.kernel.boot_process(program("other"), vec![spec("echo", &["say"], &[])], 0).unwrap();
    world.kernel.drain_outputs();
    let (echo_request, outputs) = world.call(bystander, 0, "say", &["x"]);
    let echo_dispatch = dispatches(&outputs, echo)[0].0;

    let (read, _) = world.call(agent, 0, "read", &["work", "r"]);
    let (write, _) = world.call(agent, 0, "write", &["work", "w"]);
    let (queued, outputs) = world.call(agent, 0, "write", &["work", "q"]);
    assert!(dispatches(&outputs, fs).is_empty());

    world.kernel.process_exited(fs, world.now);
    let outputs = world.kernel.drain_outputs();
    assert_eq!(code(&outputs, agent, read), Some(ErrorCode::Unavailable));
    assert_eq!(code(&outputs, agent, write), Some(ErrorCode::UnknownOutcome));
    assert_eq!(
        code(&outputs, agent, queued),
        Some(ErrorCode::Unavailable),
        "never dispatched, so known not to have run"
    );
    assert!(outputs.iter().any(|o| matches!(o, Output::Event(SecurityEvent::EndpointDown { .. }))));
    assert!(world.kernel.is_live(agent) && world.kernel.is_live(bystander) && world.kernel.is_live(echo));

    let outputs = world.send(echo, ToKernel::Reply { dispatch: echo_dispatch, result: Ok(payload()) });
    assert!(matches!(response(&outputs, bystander, echo_request), Some(Ok(_))));

    let (request, outputs) = world.call(agent, 0, "read", &["work", "r"]);
    assert_eq!(code(&outputs, agent, request), Some(ErrorCode::Unavailable));
}

#[test]
fn handles_go_stale_on_replacement_and_nothing_is_replayed() {
    let mut world = World::new();
    let (agent, old_fs) = (world.agent, world.fs);
    let (pending_write, _) = world.call(agent, 0, "write", &["work", "w"]);

    let new_fs = world
        .kernel
        .boot_process(program("fs"), vec![spec("kernel", &["provide"], &["endpoint", "fs"])], 0)
        .unwrap();
    world.kernel.drain_outputs();
    let outputs = world.send(
        new_fs,
        ToKernel::Provide { request: RequestId(1), authority: HandleId(0), interface: fs_interface() },
    );
    assert_eq!(code(&outputs, agent, pending_write), Some(ErrorCode::UnknownOutcome));
    assert_eq!(cancelled_dispatches(&outputs, old_fs).len(), 1);
    assert!(dispatches(&outputs, new_fs).is_empty(), "in-flight work is not replayed to the replacement");
    assert_eq!(world.kernel.provider_of(&name("fs")).map(|(p, g)| (p, g.0)), Some((new_fs, 2)));

    let (request, outputs) = world.call(agent, 0, "read", &["work", "r"]);
    assert_eq!(code(&outputs, agent, request), Some(ErrorCode::StaleHandle));
    assert!(dispatches(&outputs, new_fs).is_empty() && dispatches(&outputs, old_fs).is_empty());
    let outputs = world.send(agent, ToKernel::Tools { request: RequestId(4) });
    assert!(
        matches!(response(&outputs, agent, RequestId(4)), Some(Ok(ResponseBody::Tools(t))) if t.is_empty())
    );

    let outputs = world.send(agent, ToKernel::Rebind { request: RequestId(5), handle: HandleId(0) });
    assert!(matches!(response(&outputs, agent, RequestId(5)), Some(Ok(ResponseBody::Bound { .. }))));
    let (_, outputs) = world.call(agent, 0, "read", &["work", "r"]);
    assert_eq!(dispatches(&outputs, new_fs).len(), 1);

    // The displaced provider can no longer complete or receive anything.
    let (_, outputs) = world.call(agent, 0, "read", &["work", "r2"]);
    assert!(dispatches(&outputs, old_fs).is_empty());
}

#[test]
fn losing_provide_authority_unbinds_the_provider_like_an_exit() {
    for loss in ["expiry", "close"] {
        let mut world = World::new();
        let agent = world.agent;
        let parent = world
            .kernel
            .boot_process(
                program("other"),
                vec![
                    spec("kernel", &["provide"], &["endpoint", "fs"]),
                    spec("kernel", &["spawn"], &["program", "worker"]),
                ],
                0,
            )
            .unwrap();
        let mut provide = grant(0, &["provide"], &["endpoint", "fs"]);
        provide.ttl_ms = Some(50);
        let outputs = world.send(
            parent,
            ToKernel::Spawn {
                request: RequestId(1),
                authority: HandleId(1),
                program: program("worker"),
                grants: vec![provide],
            },
        );
        let Some(Ok(ResponseBody::Spawned { child })) = response(&outputs, parent, RequestId(1)) else {
            panic!("spawn failed: {outputs:?}");
        };
        world.send(
            child,
            ToKernel::Provide { request: RequestId(1), authority: HandleId(0), interface: fs_interface() },
        );
        world.send(agent, ToKernel::Rebind { request: RequestId(2), handle: HandleId(0) });
        let (inflight, outputs) = world.call(agent, 0, "read", &["work", "a"]);
        assert_eq!(dispatches(&outputs, child).len(), 1);

        let outputs = match loss {
            "expiry" => {
                world.now = 60;
                world.kernel.tick(world.now);
                world.kernel.drain_outputs()
            }
            _ => world.send(child, ToKernel::Close { request: RequestId(2), handle: HandleId(0) }),
        };
        assert_eq!(code(&outputs, agent, inflight), Some(ErrorCode::Unavailable), "{loss}");
        assert_eq!(cancelled_dispatches(&outputs, child).len(), 1, "{loss}");
        assert_eq!(world.kernel.provider_of(&name("fs")), None, "{loss}");
        let (request, outputs) = world.call(agent, 0, "read", &["work", "b"]);
        assert_eq!(code(&outputs, agent, request), Some(ErrorCode::Unavailable), "{loss}");
        assert!(dispatches(&outputs, child).is_empty(), "{loss}");
    }
}

#[test]
fn providing_requires_authority_for_that_name() {
    let mut world = World::new();
    let agent = world.agent;
    let mut interface = fs_interface();
    let outputs = world.send(
        agent,
        ToKernel::Provide { request: RequestId(1), authority: HandleId(1), interface: interface.clone() },
    );
    assert_eq!(code(&outputs, agent, RequestId(1)), Some(ErrorCode::PermissionDenied));
    assert_eq!(world.kernel.provider_of(&name("fs")).map(|(p, _)| p), Some(world.fs));

    interface.endpoint = name("shell");
    let fs = world.fs;
    let outputs =
        world.send(fs, ToKernel::Provide { request: RequestId(2), authority: HandleId(0), interface });
    assert_eq!(code(&outputs, fs, RequestId(2)), Some(ErrorCode::PermissionDenied));
}

#[test]
fn only_the_provider_can_complete_a_dispatch_and_it_cannot_forge_kernel_errors() {
    let mut world = World::new();
    let (agent, fs) = (world.agent, world.fs);
    let worker = world.worker(vec![grant(0, &["read"], &["work"])]);
    let (request, outputs) = world.call(agent, 0, "read", &["work", "a"]);
    let dispatch = dispatches(&outputs, fs)[0].0;

    let outputs = world.send(worker, ToKernel::Reply { dispatch, result: Ok(payload()) });
    assert!(response(&outputs, agent, request).is_none());

    let forged = Err(Error::new(ErrorCode::StaleHandle, "pretend"));
    let outputs = world.send(fs, ToKernel::Reply { dispatch, result: forged });
    assert_eq!(code(&outputs, agent, request), Some(ErrorCode::ServiceError));
}

// ---- lifetimes ---------------------------------------------------------------

#[test]
fn terminating_an_owner_releases_its_subtree_and_spares_others() {
    let mut config = config();
    config.max_inflight_per_endpoint = 2;
    let mut world = World::with_config(config);
    let (agent, fs) = (world.agent, world.fs);
    let worker = world.worker(vec![grant(0, &["read"], &["work"])]);
    let bystander =
        world.kernel.boot_process(program("other"), vec![spec("fs", &["read"], &["pub"])], 0).unwrap();
    world.kernel.drain_outputs();

    let (_, outputs) = world.call(worker, 0, "read", &["work", "w"]);
    let worker_dispatch = dispatches(&outputs, fs)[0].0;
    let (bystander_request, outputs) = world.call(bystander, 0, "read", &["pub", "b"]);
    let bystander_dispatch = dispatches(&outputs, fs)[0].0;

    world.kernel.process_exited(agent, world.now);
    let outputs = world.kernel.drain_outputs();
    assert_eq!(stopped(&outputs), vec![worker], "the owner waits for physical descendant cleanup");
    assert_eq!(cancelled_dispatches(&outputs, fs), vec![worker_dispatch]);
    assert!(!world.kernel.is_live(worker) && !world.kernel.is_live(agent));
    assert!(world.kernel.is_live(bystander) && world.kernel.is_live(fs));

    // A late reply to the dead worker goes nowhere; the bystander's completes.
    let outputs = reply_ok(&mut world, worker_dispatch);
    assert!(outputs.iter().all(|o| !matches!(o, Output::Send { to, .. } if *to == worker)));
    let outputs = reply_ok(&mut world, bystander_dispatch);
    assert!(matches!(response(&outputs, bystander, bystander_request), Some(Ok(_))));

    // Dead principals have no authority, whatever handle numbers they remember.
    let (_, outputs) = world.call(worker, 0, "read", &["work", "w"]);
    assert!(dispatches(&outputs, fs).is_empty());
    world.kernel.process_reaped(worker, false);
    assert_eq!(stopped(&world.kernel.drain_outputs()), vec![agent]);
}

#[test]
fn a_parent_learns_that_its_child_exited() {
    let mut world = World::new();
    let agent = world.agent;
    let worker = world.worker(vec![]);
    world.kernel.process_exited(worker, 0);
    let outputs = world.kernel.drain_outputs();
    assert!(outputs.contains(&Output::Send { to: agent, msg: FromKernel::ChildExited { child: worker } }));
    world.kernel.process_exited(worker, 0);
    assert!(world.kernel.drain_outputs().is_empty(), "teardown is idempotent");
}

#[test]
fn shutdown_releases_everything_and_admits_nothing() {
    let mut world = World::new();
    let (agent, fs) = (world.agent, world.fs);
    let worker = world.worker(vec![grant(0, &["write"], &["work"])]);
    let (write, _) = world.call(worker, 0, "write", &["work", "w"]);

    world.kernel.begin_shutdown(world.now);
    let outputs = world.kernel.drain_outputs();
    assert_eq!(stopped(&outputs), vec![worker, fs], "only logical leaves can physically stop");
    assert!(response(&outputs, worker, write).is_none(), "a terminated caller gets no response");
    assert_eq!(world.kernel.process_count(), 0);

    let refused = world.kernel.boot_process(program("agent"), vec![], world.now);
    assert_eq!(refused.map_err(|e| e.code), Err(ErrorCode::ShuttingDown));
    let (request, outputs) = world.call(agent, 0, "read", &["work", "a"]);
    assert_eq!(code(&outputs, agent, request), Some(ErrorCode::ShuttingDown));
    world.kernel.process_reaped(worker, false);
    assert_eq!(stopped(&world.kernel.drain_outputs()), vec![agent]);
}

fn started_executable(outputs: &[Output]) -> Option<(ProcessId, ProcessId, String)> {
    outputs.iter().find_map(|o| match o {
        Output::StartExecutable { process, parent, program, .. } => {
            Some((*process, *parent, program.executable.clone()))
        }
        _ => None,
    })
}

#[test]
fn a_process_starts_an_executable_within_its_own_namespace() {
    let mut world = World::new();
    let agent = world.agent;
    let child = ChildProgram {
        executable: "/plugins/tool".to_owned(),
        args: vec!["--flag".to_owned()],
        binds: vec![ChildBind { path: "/work".to_owned(), mount: None, writable: true }],
        network: false,
    };
    // Handle 2 authorizes executables under /plugins. Delegate a subset of fs.
    let outputs = world.send(
        agent,
        ToKernel::SpawnExecutable {
            request: RequestId(1),
            authority: HandleId(2),
            program: child,
            grants: vec![grant(0, &["read"], &["work"])],
            identity: None,
        },
    );
    let born = match response(&outputs, agent, RequestId(1)) {
        Some(Ok(ResponseBody::Spawned { child })) => child,
        other => panic!("spawn_executable: {other:?}"),
    };
    // The supervisor is told to start it from the executable, under the agent.
    assert_eq!(started_executable(&outputs), Some((born, agent, "/plugins/tool".to_owned())));
    // It is the agent's child, listed as its executable path, and it holds the one delegated handle.
    let list = world.send(agent, ToKernel::Children { request: RequestId(2), depth: 1 });
    let kids = match response(&list, agent, RequestId(2)) {
        Some(Ok(ResponseBody::Children(k))) => k,
        other => panic!("children: {other:?}"),
    };
    let info = kids.iter().find(|c| c.process == born).unwrap();
    assert_eq!(info.program, "/plugins/tool");
    assert_eq!(info.handles, 1);
}

#[test]
fn spawn_executable_refuses_anything_wider_than_the_caller() {
    let mut world = World::new();
    let agent = world.agent;
    let attempt = |world: &mut World, program: ChildProgram, handle: u32, grants: Vec<Grant>| -> ErrorCode {
        let outputs = world.send(
            agent,
            ToKernel::SpawnExecutable {
                request: RequestId(9),
                authority: HandleId(handle),
                program,
                grants,
                identity: None,
            },
        );
        assert!(started_executable(&outputs).is_none(), "no process may be created on refusal");
        code(&outputs, agent, RequestId(9)).expect("a refusal responds")
    };
    let ok_exec = || ChildProgram {
        executable: "/plugins/tool".to_owned(),
        args: vec![],
        binds: vec![],
        network: false,
    };
    // Executable outside the authority's resource (/plugins): here /work.
    assert_eq!(
        attempt(&mut world, ChildProgram { executable: "/work/x".to_owned(), ..ok_exec() }, 2, vec![]),
        ErrorCode::PermissionDenied
    );
    // A bind the agent cannot see.
    assert_eq!(
        attempt(
            &mut world,
            ChildProgram {
                binds: vec![ChildBind { path: "/etc".to_owned(), mount: None, writable: false }],
                ..ok_exec()
            },
            2,
            vec![],
        ),
        ErrorCode::PermissionDenied
    );
    // Writable where the agent is read-only (/plugins).
    assert_eq!(
        attempt(
            &mut world,
            ChildProgram {
                binds: vec![ChildBind { path: "/plugins".to_owned(), mount: None, writable: true }],
                ..ok_exec()
            },
            2,
            vec![],
        ),
        ErrorCode::PermissionDenied
    );
    // Network the agent does not have.
    assert_eq!(
        attempt(&mut world, ChildProgram { network: true, ..ok_exec() }, 2, vec![]),
        ErrorCode::PermissionDenied
    );
    // No authority: using the fs handle, or a spawn-program handle, not spawn_executable.
    assert_eq!(attempt(&mut world, ok_exec(), 0, vec![]), ErrorCode::PermissionDenied);
    assert_eq!(attempt(&mut world, ok_exec(), 1, vec![]), ErrorCode::PermissionDenied);
    // A widened delegation (a resource the fs handle does not cover) rejects the whole spawn.
    assert_eq!(
        attempt(&mut world, ok_exec(), 2, vec![grant(0, &["read"], &["elsewhere"])]),
        ErrorCode::PermissionDenied
    );
    assert_eq!(world.kernel.process_count(), 2, "fs and agent only; nothing was created");
}

/// A process runs as a project only when a runtime-identity spawner holds
/// `spawn_as` for exactly that project; the project reaches the supervisor,
/// every descendant inherits it, and a project's process cannot choose another
/// one.
#[test]
fn an_identity_is_chosen_only_with_spawn_as_and_is_inherited_for_good() {
    let mut config = config();
    config.projects = true;
    let plugins = || Namespace {
        binds: vec![NamespaceBind {
            path: "/plugins".to_owned(),
            source: "/plugins".into(),
            writable: false,
        }],
        network: false,
    };
    config.namespaces.insert(program("other"), plugins());
    let mut world = World::with_config(config);
    let router = world
        .kernel
        .boot_process(
            program("other"),
            vec![
                spec("kernel", &["spawn_executable"], &["executable", "plugins"]),
                spec("kernel", &["spawn_as"], &["identity", "alpha"]),
            ],
            0,
        )
        .unwrap();
    world.kernel.drain_outputs();
    // A sandboxed child names its binds; a project's process takes none.
    let exec = |binds: bool, network: bool| ChildProgram {
        executable: "/plugins/host".to_owned(),
        args: Vec::new(),
        binds: if binds {
            vec![ChildBind { path: "/plugins".to_owned(), mount: None, writable: false }]
        } else {
            Vec::new()
        },
        network,
    };
    let started = |outputs: &[Output]| {
        outputs.iter().find_map(|o| match o {
            Output::StartExecutable { process, identity, .. } => Some((*process, identity.clone())),
            _ => None,
        })
    };
    let spawn_program = |world: &mut World,
                         from: ProcessId,
                         identity: Option<(u32, String)>,
                         grants: Vec<Grant>,
                         program: ChildProgram| {
        let request = RequestId(world.next_request);
        world.next_request += 1;
        let identity = identity.map(|(authority, project)| nucleus_protocol::IdentityRequest {
            authority: HandleId(authority),
            project,
        });
        let outputs = world.send(
            from,
            ToKernel::SpawnExecutable { request, authority: HandleId(0), program, grants, identity },
        );
        (response(&outputs, from, request).expect("a spawn responds"), started(&outputs))
    };
    let spawn = |world: &mut World, from: ProcessId, identity: Option<(u32, String)>, grants: Vec<Grant>| {
        // The router's own child is sandboxed and names its bind; a
        // project's process is ordinary and has its user's network.
        let sandboxed = identity.is_none() && from == router;
        spawn_program(world, from, identity, grants, exec(sandboxed, !sandboxed))
    };
    // Not covered by the capability, not a project's name, not a spawn_as
    // handle: refused before any process exists.
    for (identity, expected) in [
        ((1, "beta".to_owned()), ErrorCode::PermissionDenied),
        ((1, "Alpha".to_owned()), ErrorCode::InvalidInput),
        ((1, "../alpha".to_owned()), ErrorCode::InvalidInput),
        ((0, "alpha".to_owned()), ErrorCode::PermissionDenied),
    ] {
        let (result, start) = spawn(&mut world, router, Some(identity.clone()), Vec::new());
        assert_eq!(result.map_err(|e| e.code).err(), Some(expected), "{identity:?}");
        assert!(start.is_none(), "{identity:?}");
    }
    let (result, start) = spawn(
        &mut world,
        router,
        Some((1, "alpha".to_owned())),
        vec![grant(0, &["spawn_executable"], &["executable", "plugins"])],
    );
    let Ok(ResponseBody::Spawned { child: host }) = result else { panic!("{result:?}") };
    assert_eq!(start, Some((host, Some("alpha".to_owned()))), "the supervisor is told the identity");
    // A project's process is in no sandbox: a bind is refused, not ignored.
    let (result, start) =
        spawn_program(&mut world, router, Some((1, "alpha".to_owned())), Vec::new(), exec(true, true));
    assert_eq!(result.map_err(|e| e.code).err(), Some(ErrorCode::InvalidInput));
    assert!(start.is_none());
    // Its own children are the project's too, and it cannot pick another identity.
    let (result, start) = spawn(&mut world, host, None, Vec::new());
    let Ok(ResponseBody::Spawned { child }) = result else { panic!("{result:?}") };
    assert_eq!(start, Some((child, Some("alpha".to_owned()))));
    let (result, start) = spawn(&mut world, host, Some((0, "beta".to_owned())), Vec::new());
    assert_eq!(result.map_err(|e| e.code).err(), Some(ErrorCode::PermissionDenied));
    assert!(start.is_none());
    // It has its user's network, which its spawner's sandbox need not have,
    // and so have its children: one without is refused, not ignored.
    let (result, start) = spawn_program(&mut world, host, None, Vec::new(), exec(false, false));
    assert_eq!(result.map_err(|e| e.code).err(), Some(ErrorCode::InvalidInput));
    assert!(start.is_none());
    // Only the holder of `spawn_as` starts a project's process without
    // network (the discovery workers' shell), and that one starts no process
    // of its own: its child would have the network it was denied.
    let (result, start) =
        spawn_program(&mut world, router, Some((1, "alpha".to_owned())), Vec::new(), exec(false, false));
    let Ok(ResponseBody::Spawned { child: cut_off }) = result else { panic!("{result:?}") };
    assert_eq!(start, Some((cut_off, Some("alpha".to_owned()))));
    for network in [false, true] {
        let (result, start) = spawn_program(&mut world, cut_off, None, Vec::new(), exec(false, network));
        assert_eq!(result.map_err(|e| e.code).err(), Some(ErrorCode::PermissionDenied), "{network}");
        assert!(start.is_none());
    }
    // A configured program is a sandbox recipe of the runtime's own: a
    // project's process starts none.
    let request = RequestId(world.next_request);
    world.next_request += 1;
    let configured =
        ToKernel::Spawn { request, authority: HandleId(0), program: program("other"), grants: Vec::new() };
    let outputs = world.send(host, configured);
    let refused = response(&outputs, host, request).expect("a spawn responds");
    assert_eq!(refused.map_err(|e| e.code).err(), Some(ErrorCode::PermissionDenied));
    assert!(!outputs.iter().any(|o| matches!(o, Output::StartProcess { .. })));
    // The runtime's own processes stay the runtime's.
    let (result, start) = spawn(&mut world, router, None, Vec::new());
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(start.map(|s| s.1), Some(None));
}

/// A router of a VM with projects: `spawn_executable` on `/plugins`
/// (handle 0), `spawn_as` on every project (1), and, when asked,
/// `stop_project` on every project (2).
fn project_world(stop: bool) -> (World, ProcessId) {
    let mut config = config();
    config.projects = true;
    config.namespaces.insert(
        program("other"),
        Namespace {
            binds: vec![NamespaceBind {
                path: "/plugins".to_owned(),
                source: "/plugins".into(),
                writable: false,
            }],
            network: false,
        },
    );
    let mut world = World::with_config(config);
    let mut grants = vec![
        spec("kernel", &["spawn_executable"], &["executable", "plugins"]),
        spec("kernel", &["spawn_as"], &["identity"]),
    ];
    if stop {
        grants.push(spec("kernel", &["stop_project"], &["identity"]));
    }
    let router = world.kernel.boot_process(program("other"), grants, 0).unwrap();
    world.kernel.drain_outputs();
    (world, router)
}

/// Starts `/plugins/host` as `project` from `from`, with handle 0 delegated
/// so it can start its own.
fn start_as(world: &mut World, from: ProcessId, project: &str) -> Result<ProcessId, Error> {
    let request = RequestId(world.next_request);
    world.next_request += 1;
    let identity =
        Some(nucleus_protocol::IdentityRequest { authority: HandleId(1), project: project.to_owned() });
    let program = ChildProgram {
        executable: "/plugins/host".to_owned(),
        args: Vec::new(),
        binds: Vec::new(),
        network: true,
    };
    let grants = vec![grant(0, &["spawn_executable"], &["executable", "plugins"])];
    let outputs = world
        .send(from, ToKernel::SpawnExecutable { request, authority: HandleId(0), program, grants, identity });
    match response(&outputs, from, request).expect("a spawn responds") {
        Ok(ResponseBody::Spawned { child }) => Ok(child),
        Ok(other) => panic!("{other:?}"),
        Err(e) => Err(e),
    }
}

/// Stopping every process of a project: only a runtime process holding
/// `stop_project` on the project asks, naming a project; the supervisor is
/// told and the answer comes when it reports back, to the asker only while
/// it lives; a few at once.
#[test]
fn a_project_is_stopped_only_with_stop_project_and_answered_when_done() {
    use nucleus_protocol::ProjectStopped;
    let ask = |world: &mut World, from: ProcessId, authority: u32, project: &str| {
        let request = RequestId(world.next_request);
        world.next_request += 1;
        let outputs = world.send(
            from,
            ToKernel::StopProject { request, authority: HandleId(authority), project: project.to_owned() },
        );
        let told =
            outputs.iter().any(|o| matches!(o, Output::StopProject { request: r, .. } if *r == request));
        (request, response(&outputs, from, request), told)
    };
    let (mut world, router) = project_world(false);
    let (_, refused, told) = ask(&mut world, router, 1, "alpha");
    assert_eq!(
        refused.unwrap().map_err(|e| e.code).err(),
        Some(ErrorCode::PermissionDenied),
        "spawn_as is not enough"
    );
    assert!(!told);

    let (mut world, router) = project_world(true);
    for bad in ["", ".deleting-alpha", "a/b", "-x", "Alpha"] {
        let (_, refused, told) = ask(&mut world, router, 2, bad);
        assert!(refused.is_some_and(|r| r.is_err()) && !told, "{bad:?}");
    }
    let (request, pending, told) = ask(&mut world, router, 2, "alpha");
    assert!(pending.is_none() && told, "asked of the supervisor, answered later");
    world.kernel.project_stopped(router, request, Ok(()));
    let answered = world.kernel.drain_outputs();
    let Some(Ok(ResponseBody::Payload(payload))) = response(&answered, router, request) else {
        panic!("{answered:?}")
    };
    assert_eq!(payload.decode::<ProjectStopped>().unwrap(), ProjectStopped {});
    // Reported twice, or for a request never made: nothing.
    world.kernel.project_stopped(router, request, Ok(()));
    world.kernel.project_stopped(router, RequestId(999), Ok(()));
    assert!(world.kernel.drain_outputs().is_empty());
    // A project's process stops no project, even holding the capability.
    let project = start_as(&mut world, router, "alpha").unwrap();
    let (_, refused, told) = ask(&mut world, project, 2, "beta");
    assert!(refused.is_some_and(|r| r.is_err()) && !told);
    // A few at once.
    let mut asked = Vec::new();
    for n in 0..nucleus_kernel::MAX_PROJECT_STOPS {
        let (request, pending, told) = ask(&mut world, router, 2, &format!("p{n}"));
        assert!(pending.is_none() && told);
        asked.push(request);
    }
    let (_, busy, told) = ask(&mut world, router, 2, "one-more");
    assert_eq!(busy.unwrap().map_err(|e| e.code).err(), Some(ErrorCode::LimitExceeded));
    assert!(!told);
    // A failure is the asker's answer; once it is gone, nobody is answered.
    world.kernel.project_stopped(router, asked[0], Err(Error::new(ErrorCode::ServiceError, "still running")));
    let failed = world.kernel.drain_outputs();
    assert_eq!(
        response(&failed, router, asked[0]).unwrap().map_err(|e| e.code).err(),
        Some(ErrorCode::ServiceError)
    );
    world.kernel.process_exited(router, world.now);
    world.kernel.drain_outputs();
    world.kernel.project_stopped(router, asked[1], Ok(()));
    assert!(response(&world.kernel.drain_outputs(), router, asked[1]).is_none());
}

#[test]
fn tools_lists_held_tool_actions_of_a_live_provider() {
    let mut world = World::new();
    let agent = world.agent; // holds fs read+write on /work
    let outputs = world.send(agent, ToKernel::Tools { request: RequestId(1) });
    let tools = match response(&outputs, agent, RequestId(1)) {
        Some(Ok(ResponseBody::Tools(tools))) => tools,
        other => panic!("tools: {other:?}"),
    };
    // `read` is declared a tool; `write` is internal; the kernel-authority
    // handles (spawn) provide nothing tool-callable. So exactly one tool.
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!((tools[0].endpoint.as_str(), tools[0].action.as_str()), ("fs", "read"));
    assert_eq!(tools[0].resource.segments(), ["work"], "the tool carries the handle's scope");
    assert_eq!(tools[0].description, "read a file");
    assert!(matches!(&tools[0].resource_from, ResourceFrom::Segments { param } if param == "path"));

    // The fs provider itself holds only `kernel provide` (no tool); it sees none.
    let fs = world.fs;
    let outputs = world.send(fs, ToKernel::Tools { request: RequestId(2) });
    assert!(matches!(response(&outputs, fs, RequestId(2)), Some(Ok(ResponseBody::Tools(t))) if t.is_empty()));
}

#[test]
fn children_lists_only_the_callers_own_subtree() {
    let mut world = World::new();
    let (agent, fs) = (world.agent, world.fs);
    // Give the worker a spawn capability so it can have a child of its own.
    let worker = world.worker(vec![grant(1, &["spawn"], &["program", "worker"])]);
    let bystander = world.kernel.boot_process(program("other"), vec![], 0).unwrap();
    // The worker spawns a grandchild (of the agent) with no grants.
    let outputs = world.send(
        worker,
        ToKernel::Spawn {
            request: RequestId(1),
            authority: HandleId(0),
            program: program("worker"),
            grants: vec![],
        },
    );
    let grandchild = match response(&outputs, worker, RequestId(1)) {
        Some(Ok(ResponseBody::Spawned { child })) => child,
        other => panic!("worker spawn: {other:?}"),
    };

    let list = |world: &mut World, from: ProcessId, depth: u32| -> Vec<ChildInfo> {
        let outputs = world.send(from, ToKernel::Children { request: RequestId(7), depth });
        match response(&outputs, from, RequestId(7)) {
            Some(Ok(ResponseBody::Children(list))) => list,
            other => panic!("children: {other:?}"),
        }
    };

    // Depth 1 from the agent: the worker only, not the grandchild, and never fs or the bystander.
    let direct = list(&mut world, agent, 1);
    assert_eq!(direct.iter().map(|c| c.process).collect::<Vec<_>>(), vec![worker]);
    assert_eq!(direct[0].program, "worker");
    assert_eq!(direct[0].children, 1);

    // Depth 2 reaches the grandchild.
    let deep = list(&mut world, agent, 2);
    let ids: Vec<_> = deep.iter().map(|c| c.process).collect();
    assert!(ids.contains(&worker) && ids.contains(&grandchild), "{ids:?}");

    // The worker sees its own child; the grandchild, fs and the bystander see nothing.
    assert_eq!(list(&mut world, worker, 5).iter().map(|c| c.process).collect::<Vec<_>>(), vec![grandchild]);
    assert!(list(&mut world, grandchild, 5).is_empty());
    assert!(list(&mut world, fs, 5).is_empty());
    assert!(list(&mut world, bystander, 5).is_empty(), "a stranger's subtree is empty, not the whole tree");
}

#[test]
fn a_parent_can_kill_its_child_and_nobody_elses() {
    let mut world = World::new();
    let (agent, fs) = (world.agent, world.fs);
    let worker = world.worker(vec![grant(0, &["read"], &["work"])]);
    let bystander = world.kernel.boot_process(program("other"), vec![], 0).unwrap();
    world.kernel.drain_outputs();

    // Not the parent, a grandparent-less stranger, the child itself, a made-up id: all the same answer.
    for (from, target) in
        [(bystander, worker), (fs, agent), (worker, worker), (worker, agent), (agent, ProcessId(999))]
    {
        let outputs = world.send(from, ToKernel::Kill { request: RequestId(1), child: target });
        assert_eq!(code(&outputs, from, RequestId(1)), Some(ErrorCode::PermissionDenied));
        assert!(stopped(&outputs).is_empty());
        assert!(denied(&outputs, from, ErrorCode::PermissionDenied));
    }
    assert!(world.kernel.is_live(worker));

    let outputs = world.send(agent, ToKernel::Kill { request: RequestId(2), child: worker });
    assert!(matches!(response(&outputs, agent, RequestId(2)), Some(Ok(ResponseBody::Done))));
    assert_eq!(stopped(&outputs), vec![worker]);
    assert!(outputs.contains(&Output::Send { to: agent, msg: FromKernel::ChildExited { child: worker } }));
    let (request, outputs) = world.call(worker, 0, "read", &["work", "a"]);
    assert_eq!(code(&outputs, worker, request), Some(ErrorCode::ShuttingDown));
    assert!(dispatches(&outputs, fs).is_empty(), "a killed child has no authority left");
    assert!(world.kernel.is_live(agent) && world.kernel.is_live(bystander));
}

// ---- bounds ------------------------------------------------------------------

#[test]
fn queues_and_outstanding_requests_are_bounded() {
    let mut world = World::new();
    let (agent, fs) = (world.agent, world.fs);
    let (_, outputs) = world.call(agent, 0, "read", &["work", "inflight"]);
    assert_eq!(dispatches(&outputs, fs).len(), 1);
    world.call(agent, 0, "read", &["work", "q1"]);
    world.call(agent, 0, "read", &["work", "q2"]);
    let (overflow, outputs) = world.call(agent, 0, "read", &["work", "q3"]);
    assert_eq!(code(&outputs, agent, overflow), Some(ErrorCode::QueueFull));

    let mut config = config();
    config.max_pending_per_caller = 1;
    let mut world = World::with_config(config);
    let agent = world.agent;
    world.call(agent, 0, "read", &["work", "a"]);
    let (second, outputs) = world.call(agent, 0, "read", &["work", "b"]);
    assert_eq!(code(&outputs, agent, second), Some(ErrorCode::LimitExceeded));
}

#[test]
fn deadlines_and_cancellation_report_what_is_actually_known() {
    let mut world = World::new();
    let (agent, fs) = (world.agent, world.fs);
    let (write, outputs) = world.call(agent, 0, "write", &["work", "w"]);
    let write_dispatch = dispatches(&outputs, fs)[0].0;
    let (queued_read, _) = world.call(agent, 0, "read", &["work", "r"]);
    let (cancelled, _) = world.call(agent, 0, "read", &["work", "c"]);

    let outputs = world.send(agent, ToKernel::Cancel { request: cancelled });
    assert_eq!(code(&outputs, agent, cancelled), Some(ErrorCode::Cancelled));

    world.now = 1_000;
    world.kernel.tick(world.now);
    let outputs = world.kernel.drain_outputs();
    assert_eq!(
        code(&outputs, agent, write),
        Some(ErrorCode::UnknownOutcome),
        "a dispatched mutation may have happened"
    );
    assert_eq!(code(&outputs, agent, queued_read), Some(ErrorCode::Timeout));
    assert_eq!(cancelled_dispatches(&outputs, fs), vec![write_dispatch]);
    assert!(dispatches(&outputs, fs).is_empty());

    // The late reply is dropped and the slot is usable again.
    assert!(reply_ok(&mut world, write_dispatch).is_empty());
    let (_, outputs) = world.call(agent, 0, "read", &["work", "again"]);
    assert_eq!(dispatches(&outputs, fs).len(), 1);
}

#[test]
fn invalid_configuration_is_refused() {
    let mut config = config();
    config.max_queue_per_endpoint = 0;
    assert!(Kernel::new(config).is_err());
    let mut config = self::config();
    config.max_pending_per_caller = 0;
    assert!(Kernel::new(config).is_err());
}

#[test]
fn configured_spawn_cannot_add_network_or_host_filesystem_authority() {
    for namespace in [
        Namespace { binds: vec![], network: true },
        Namespace {
            binds: vec![NamespaceBind { path: "/work".into(), source: "/private".into(), writable: false }],
            network: false,
        },
        Namespace {
            binds: vec![NamespaceBind { path: "/renamed".into(), source: "/plugins".into(), writable: true }],
            network: false,
        },
    ] {
        let mut cfg = config();
        cfg.namespaces.insert(program("worker"), namespace);
        let mut kernel = Kernel::new(cfg).unwrap();
        let parent = kernel
            .boot_process(program("agent"), vec![spec("kernel", &["spawn"], &["program", "worker"])], 0)
            .unwrap();
        kernel.drain_outputs();
        kernel.handle_message(
            parent,
            ToKernel::Spawn {
                request: RequestId(1),
                authority: HandleId(0),
                program: program("worker"),
                grants: vec![],
            },
            1,
        );
        let outputs = kernel.drain_outputs();
        assert_eq!(code(&outputs, parent, RequestId(1)), Some(ErrorCode::PermissionDenied));
        assert!(!outputs.iter().any(|o| matches!(o, Output::StartProcess { .. })));
        assert_eq!(kernel.process_count(), 1);
    }
}

#[test]
fn a_reaped_child_is_successful_only_after_an_ordinary_exit_and_zero_status() {
    for (ordinary, host_success) in [(true, true), (true, false), (false, true)] {
        let mut world = World::new();
        let child = world.worker(vec![]);
        world.kernel.drain_outputs();
        if ordinary {
            world.kernel.process_disconnected(child, world.now);
        } else {
            world.kernel.process_exited(child, world.now);
        }
        let outputs = world.kernel.drain_outputs();
        assert!(outputs.iter().any(|o| matches!(o, Output::Send { to, msg: FromKernel::ChildExited { child: c } } if *to == world.agent && *c == child)));
        assert!(
            !outputs.iter().any(|o| matches!(o, Output::Send { msg: FromKernel::ChildReaped { .. }, .. }))
        );
        world.kernel.process_reaped(child, host_success);
        let outputs = world.kernel.drain_outputs();
        assert!(outputs.iter().any(|o| matches!(o, Output::Send { to, msg: FromKernel::ChildReaped { child: c, success, cleanup_confirmed: true } } if *to == world.agent && *c == child && *success == (ordinary && host_success))));
        world.kernel.process_reaped(child, true);
        assert!(world.kernel.drain_outputs().is_empty());
    }
}

#[test]
fn an_intentionally_stopped_child_has_confirmed_cleanup_only_after_host_reap() {
    let mut world = World::new();
    let child = world.worker(vec![]);
    world.kernel.drain_outputs();
    let outputs = world.send(world.agent, ToKernel::Kill { request: RequestId(20), child });
    assert!(
        outputs.iter().any(|output| matches!(output, Output::StopProcess { process } if *process == child))
    );
    assert!(
        !outputs
            .iter()
            .any(|output| matches!(output, Output::Send { msg: FromKernel::ChildReaped { .. }, .. }))
    );
    world.kernel.tick(world.now + 100_000);
    assert!(
        !world
            .kernel
            .drain_outputs()
            .iter()
            .any(|output| matches!(output, Output::Send { msg: FromKernel::ChildReaped { .. }, .. })),
        "elapsed time cannot attest cleanup"
    );
    world.kernel.process_reaped(child, false);
    let outputs = world.kernel.drain_outputs();
    assert!(outputs.iter().any(|output| matches!(output, Output::Send { to, msg: FromKernel::ChildReaped { child: observed, success: false, cleanup_confirmed: true } } if *to == world.agent && *observed == child)));
}

#[test]
fn a_child_awaiting_host_reap_still_occupies_its_process_budget() {
    let mut cfg = config();
    cfg.max_processes = 2;
    let mut kernel = Kernel::new(cfg).unwrap();
    let parent = kernel
        .boot_process(program("agent"), vec![spec("kernel", &["spawn"], &["program", "worker"])], 0)
        .unwrap();
    kernel.drain_outputs();
    let spawn = |request| ToKernel::Spawn {
        request: RequestId(request),
        authority: HandleId(0),
        program: program("worker"),
        grants: vec![],
    };
    kernel.handle_message(parent, spawn(1), 0);
    let outputs = kernel.drain_outputs();
    let child = outputs
        .iter()
        .find_map(|o| if let Output::StartProcess { process, .. } = o { Some(*process) } else { None })
        .unwrap();
    kernel.process_disconnected(child, 1);
    kernel.drain_outputs();
    kernel.handle_message(parent, spawn(2), 1);
    assert_eq!(code(&kernel.drain_outputs(), parent, RequestId(2)), Some(ErrorCode::LimitExceeded));
    kernel.process_reaped(child, true);
    kernel.drain_outputs();
    kernel.handle_message(parent, spawn(3), 2);
    assert!(kernel.drain_outputs().iter().any(|o| matches!(o, Output::StartProcess { .. })));
}

#[test]
fn subtree_cleanup_waits_for_every_host_in_either_reap_order() {
    for ordinary in [false, true] {
        for parent_first in [false, true] {
            for descendant_already_exited in [false, true] {
                let mut world = World::new();
                let child = world.worker(vec![grant(1, &["spawn"], &["program", "worker"])]);
                let outputs = world.send(
                    child,
                    ToKernel::Spawn {
                        request: RequestId(30),
                        authority: HandleId(0),
                        program: program("worker"),
                        grants: vec![],
                    },
                );
                let grandchild = match response(&outputs, child, RequestId(30)).unwrap().unwrap() {
                    ResponseBody::Spawned { child } => child,
                    other => panic!("unexpected spawn response: {other:?}"),
                };
                if descendant_already_exited {
                    world.kernel.process_disconnected(grandchild, 1);
                }
                if ordinary {
                    world.kernel.process_disconnected(child, 2);
                } else {
                    world.kernel.process_exited(child, 2);
                }
                world.kernel.drain_outputs();
                let (first, last) = if parent_first { (child, grandchild) } else { (grandchild, child) };
                world.kernel.process_reaped(first, true);
                let outputs = world.kernel.drain_outputs();
                assert!(
                    !outputs
                        .iter()
                        .any(|o| matches!(o, Output::Send { msg: FromKernel::ChildReaped { .. }, .. })),
                    "a live parent must not receive partial subtree cleanup"
                );
                assert_eq!(stopped(&outputs), if parent_first { vec![] } else { vec![child] });
                // Duplicate host evidence and elapsed time cannot substitute
                // for the missing host's cleanup acknowledgment.
                world.kernel.process_reaped(first, false);
                world.kernel.tick(100_000);
                assert!(world.kernel.drain_outputs().is_empty());
                world.kernel.process_reaped(last, true);
                let outputs = world.kernel.drain_outputs();
                assert_eq!(outputs.len(), 1);
                assert!(
                    matches!(&outputs[0], Output::Send { to, msg: FromKernel::ChildReaped { child: observed, success, cleanup_confirmed: true } } if *to == world.agent && *observed == child && *success == ordinary)
                );
                world.kernel.process_reaped(child, true);
                world.kernel.process_reaped(grandchild, true);
                assert!(world.kernel.drain_outputs().is_empty());
            }
        }
    }
}

#[test]
fn an_unreaped_boot_process_keeps_its_reservation() {
    let mut cfg = Config::with_programs([program("agent")].into_iter().collect());
    cfg.max_processes = 1;
    let mut kernel = Kernel::new(cfg).unwrap();
    let agent = kernel.boot_process(program("agent"), vec![], 0).unwrap();
    kernel.drain_outputs();
    kernel.process_exited(agent, 1);
    assert_eq!(kernel.boot_process(program("agent"), vec![], 2).unwrap_err().code, ErrorCode::LimitExceeded);
    kernel.process_reaped(agent, false);
    assert!(kernel.boot_process(program("agent"), vec![], 3).is_ok());
}

fn child_and_grandchild(world: &mut World) -> (ProcessId, ProcessId) {
    let child = world.worker(vec![grant(1, &["spawn"], &["program", "worker"])]);
    let outputs = world.send(
        child,
        ToKernel::Spawn {
            request: RequestId(700),
            authority: HandleId(0),
            program: program("worker"),
            grants: vec![],
        },
    );
    let grandchild = match response(&outputs, child, RequestId(700)).unwrap().unwrap() {
        ResponseBody::Spawned { child } => child,
        other => panic!("unexpected spawn response: {other:?}"),
    };
    (child, grandchild)
}

#[test]
fn requested_termination_closes_the_subtree_but_stops_only_confirmed_leaves() {
    let mut world = World::new();
    let (child, grandchild) = child_and_grandchild(&mut world);
    let outputs = world.send(
        child,
        ToKernel::Spawn {
            request: RequestId(701),
            authority: HandleId(0),
            program: program("worker"),
            grants: vec![],
        },
    );
    let sibling = match response(&outputs, child, RequestId(701)).unwrap().unwrap() {
        ResponseBody::Spawned { child } => child,
        other => panic!("unexpected spawn response: {other:?}"),
    };
    let outputs = world.send(world.agent, ToKernel::Kill { request: RequestId(702), child });
    assert_eq!(stopped(&outputs), vec![sibling, grandchild]);
    for process in [child, grandchild, sibling] {
        assert!(!world.kernel.is_live(process));
        assert!(world.kernel.is_closing(process));
        assert!(!world.kernel.cleanup_confirmed(process));
    }
    assert!(world.kernel.is_live(world.agent));
    assert!(world.kernel.is_live(world.fs));
    let outputs = world.send(
        child,
        ToKernel::Spawn {
            request: RequestId(703),
            authority: HandleId(0),
            program: program("worker"),
            grants: vec![],
        },
    );
    assert_eq!(code(&outputs, child, RequestId(703)), Some(ErrorCode::ShuttingDown));
    assert!(!outputs.iter().any(|o| matches!(o, Output::StartProcess { .. })));
    let (request, outputs) = world.call(child, 0, "read", &["work"]);
    assert_eq!(code(&outputs, child, request), Some(ErrorCode::ShuttingDown));
    assert!(dispatches(&outputs, world.fs).is_empty());
    world.kernel.process_reaped(grandchild, false);
    assert!(world.kernel.drain_outputs().is_empty());
    assert!(world.kernel.cleanup_confirmed(grandchild));
    world.kernel.process_reaped(grandchild, true);
    world.kernel.tick(1_000_000);
    assert!(world.kernel.drain_outputs().is_empty(), "no duplicate stop or inferred cleanup");
    assert!(world.kernel.is_closing(child));
    world.kernel.process_reaped(sibling, true);
    assert_eq!(stopped(&world.kernel.drain_outputs()), vec![child]);
    assert!(!world.kernel.cleanup_confirmed(child));
    world.kernel.process_reaped(child, false);
    let outputs = world.kernel.drain_outputs();
    assert!(outputs.contains(&Output::Send {
        to: world.agent,
        msg: FromKernel::ChildReaped { child, success: false, cleanup_confirmed: true },
    }));
    assert!(world.kernel.cleanup_confirmed(child));
    assert!(!world.kernel.is_closing(child));
    assert!(!world.kernel.cleanup_confirmed(ProcessId(0)));
    assert!(!world.kernel.cleanup_confirmed(ProcessId(u64::MAX)));
}

#[test]
fn cooperative_exit_acknowledges_only_after_descendant_cleanup_and_preserves_success() {
    let mut world = World::new();
    let (child, grandchild) = child_and_grandchild(&mut world);
    let request = RequestId(710);
    let outputs = world.send(child, ToKernel::Exit { request });
    assert_eq!(stopped(&outputs), vec![grandchild]);
    assert!(response(&outputs, child, request).is_none());
    assert!(world.kernel.is_closing(child));
    world.kernel.tick(1_000_000);
    assert!(world.kernel.drain_outputs().is_empty());
    world.kernel.process_reaped(grandchild, false);
    let outputs = world.kernel.drain_outputs();
    assert_eq!(
        outputs,
        vec![
            Output::Send { to: child, msg: FromKernel::Response { request, result: Ok(ResponseBody::Done) } },
            Output::StopProcess { process: child },
        ]
    );
    world.kernel.process_disconnected(child, 1_000_001);
    assert!(world.kernel.drain_outputs().is_empty(), "EOF after an Exit is idempotent");
    world.kernel.process_reaped(child, true);
    assert_eq!(
        world.kernel.drain_outputs(),
        vec![Output::Send {
            to: world.agent,
            msg: FromKernel::ChildReaped { child, success: true, cleanup_confirmed: true },
        }]
    );
}

#[test]
fn a_crashed_parent_receipt_cannot_substitute_for_descendant_cleanup() {
    let mut world = World::new();
    let (child, grandchild) = child_and_grandchild(&mut world);
    world.kernel.process_exited(child, 1);
    assert_eq!(stopped(&world.kernel.drain_outputs()), vec![grandchild]);
    world.kernel.process_reaped(child, false);
    world.kernel.tick(1_000_000);
    assert!(world.kernel.drain_outputs().is_empty());
    assert!(world.kernel.is_closing(child));
    assert!(!world.kernel.cleanup_confirmed(child));
    world.kernel.process_reaped(grandchild, true);
    assert_eq!(
        world.kernel.drain_outputs(),
        vec![Output::Send {
            to: world.agent,
            msg: FromKernel::ChildReaped { child, success: false, cleanup_confirmed: true },
        }]
    );
    assert!(world.kernel.cleanup_confirmed(child));
}
