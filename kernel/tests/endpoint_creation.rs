//! Fresh objects are derived from a factory; ordinary capabilities never gain
//! authority over an existing object by changing an endpoint name.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use nucleus_kernel::{CapSpec, Config, Kernel, Output, SecurityEvent};
use nucleus_protocol::{
    ActionDecl, ActionName, CreatedEndpoint, EffectClass, EndpointName, Error, ErrorCode, FromKernel, Grant,
    HandleId, InterfaceDecl, MAX_ENDPOINT_ACTIONS, Payload, ProcessId, ProgramName, RequestId, ResourceFrom,
    ResourcePath, ResponseBody, ToKernel,
};
use serde_json::json;

fn actions(values: &[&str]) -> BTreeSet<ActionName> {
    values.iter().map(|value| ActionName::new(*value).unwrap()).collect()
}

fn path(values: &[&str]) -> ResourcePath {
    ResourcePath::new(values.iter().copied()).unwrap()
}

fn spec(endpoint: &str, wanted: &[&str], resource: &[&str]) -> CapSpec {
    CapSpec {
        endpoint: EndpointName::new(endpoint).unwrap(),
        actions: actions(wanted),
        resource: path(resource),
        ttl_ms: None,
        delegable: true,
        max_calls: None,
    }
}

fn factory() -> CapSpec {
    spec("kernel", &["create_endpoint"], &["endpoint"])
}

fn config() -> Config {
    let mut config = Config::with_programs(BTreeSet::from([ProgramName::new("agent").unwrap()]));
    config.max_inflight_per_endpoint = 1;
    config
}

struct World {
    kernel: Kernel,
    owner: ProcessId,
    now: u64,
    next: u64,
    outputs: Vec<Output>,
}

impl World {
    fn new(config: Config, factory: CapSpec) -> Self {
        let mut kernel = Kernel::new(config).unwrap();
        let owner = kernel
            .boot_process(
                ProgramName::new("agent").unwrap(),
                vec![factory, spec("kernel", &["spawn"], &["program", "agent"])],
                0,
            )
            .unwrap();
        kernel.drain_outputs();
        Self { kernel, owner, now: 0, next: 1, outputs: Vec::new() }
    }

    fn send(&mut self, from: ProcessId, message: impl FnOnce(RequestId) -> ToKernel) -> RequestId {
        let request = RequestId(self.next);
        self.next += 1;
        self.kernel.handle_message(from, message(request), self.now);
        self.outputs = self.kernel.drain_outputs();
        request
    }

    fn response(&self, request: RequestId) -> Result<ResponseBody, Error> {
        self.outputs
            .iter()
            .find_map(|output| match output {
                Output::Send { msg: FromKernel::Response { request: got, result }, .. }
                    if *got == request =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .unwrap()
    }

    fn rpc(
        &mut self,
        from: ProcessId,
        message: impl FnOnce(RequestId) -> ToKernel,
    ) -> Result<ResponseBody, Error> {
        let request = self.send(from, message);
        self.response(request)
    }

    fn create(
        &mut self,
        owner: ProcessId,
        authority: HandleId,
        wanted: BTreeSet<ActionName>,
    ) -> Result<CreatedEndpoint, Error> {
        self.rpc(owner, |request| ToKernel::CreateEndpoint { request, authority, actions: wanted }).map(
            |body| match body {
                ResponseBody::EndpointCreated(created) => *created,
                _ => panic!("unexpected reply"),
            },
        )
    }

    fn allocate(&mut self) -> CreatedEndpoint {
        self.create(self.owner, HandleId(0), actions(&["run"])).unwrap()
    }

    fn spawn(&mut self, grants: Vec<Grant>) -> Result<ProcessId, Error> {
        self.rpc(self.owner, |request| ToKernel::Spawn {
            request,
            authority: HandleId(1),
            program: ProgramName::new("agent").unwrap(),
            grants,
        })
        .map(|body| match body {
            ResponseBody::Spawned { child } => child,
            _ => panic!("unexpected reply"),
        })
    }

    fn publish(
        &mut self,
        from: ProcessId,
        authority: HandleId,
        endpoint: &EndpointName,
    ) -> Result<ResponseBody, Error> {
        self.rpc(from, |request| ToKernel::Provide { request, authority, interface: interface(endpoint) })
    }

    fn rebind(&mut self, from: ProcessId, handle: HandleId) -> Result<ResponseBody, Error> {
        self.rpc(from, |request| ToKernel::Rebind { request, handle })
    }

    fn call(&mut self, from: ProcessId, handle: HandleId, act: &str, resource: ResourcePath) -> RequestId {
        self.send(from, |request| ToKernel::Call {
            request,
            handle,
            action: ActionName::new(act).unwrap(),
            resource,
            payload: Payload::encode(&json!({"work":true})).unwrap(),
            deadline_ms: None,
        })
    }

    fn child_provider(&mut self, created: &CreatedEndpoint) -> ProcessId {
        let child = self.spawn(vec![grant(&created.provide, created.provide.resource.clone())]).unwrap();
        self.publish(child, HandleId(0), &created.endpoint).unwrap();
        self.rebind(self.owner, created.invoke.handle).unwrap();
        child
    }
}

fn grant(source: &nucleus_protocol::HandleInfo, resource: ResourcePath) -> Grant {
    Grant {
        from: source.handle,
        actions: source.actions.clone(),
        resource,
        ttl_ms: None,
        delegable: false,
        max_calls: None,
    }
}

fn interface(endpoint: &EndpointName) -> InterfaceDecl {
    InterfaceDecl {
        endpoint: endpoint.clone(),
        version: 1,
        description: "owned plugin".into(),
        actions: BTreeMap::from([(
            ActionName::new("run").unwrap(),
            ActionDecl::tool(
                EffectClass::ReversibleMutation,
                "run",
                json!({"type":"object"}),
                ResourceFrom::Handle,
            ),
        )]),
    }
}

#[test]
fn a_factory_allocates_delegable_handles_and_a_child_can_publish_and_serve_them() {
    let mut world = World::new(config(), factory());
    let created = world.allocate();
    assert_eq!(created.provide.endpoint.as_str(), "kernel");
    assert_eq!(created.provide.actions, actions(&["provide"]));
    assert_eq!(created.provide.resource, path(&["endpoint", created.endpoint.as_str()]));
    assert_eq!(created.invoke.endpoint, created.endpoint);
    assert_eq!(created.invoke.actions, actions(&["run"]));
    assert!(created.provide.delegable && created.invoke.delegable);
    assert!(world.outputs.iter().any(|output| matches!(output, Output::Event(SecurityEvent::EndpointCreated { owner, endpoint, .. }) if *owner == world.owner && *endpoint == created.endpoint)));
    let provider = world.child_provider(&created);
    let consumer = world.spawn(vec![grant(&created.invoke, path(&["allowed"]))]).unwrap();
    let denied = world.call(consumer, HandleId(0), "run", path(&["elsewhere"]));
    assert_eq!(world.response(denied).unwrap_err().code, ErrorCode::PermissionDenied);
    let call = world.call(consumer, HandleId(0), "run", path(&["allowed", "task"]));
    let dispatch = world
        .outputs
        .iter()
        .find_map(|output| match output {
            Output::Send { to, msg: FromKernel::Dispatch { dispatch, resource, caller, .. } }
                if *to == provider && *caller == consumer && *resource == path(&["allowed", "task"]) =>
            {
                Some(*dispatch)
            }
            _ => None,
        })
        .unwrap();
    world.kernel.handle_message(
        provider,
        ToKernel::Reply { dispatch, result: Ok(Payload::encode(&json!("done")).unwrap()) },
        0,
    );
    world.outputs = world.kernel.drain_outputs();
    assert!(matches!(world.response(call), Ok(ResponseBody::Payload(_))));
}

#[test]
fn fabricated_wrong_scoped_and_non_delegable_factories_are_refused() {
    let mut world = World::new(config(), factory());
    assert_eq!(
        world.create(world.owner, HandleId(99), actions(&["run"])).unwrap_err().code,
        ErrorCode::UnknownHandle
    );
    assert_eq!(
        world.create(world.owner, HandleId(1), actions(&["run"])).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    let mut nondelegable = factory();
    nondelegable.delegable = false;
    for spec in [nondelegable, spec("kernel", &["create_endpoint"], &["endpoint", "other"])] {
        let mut world = World::new(config(), spec);
        assert_eq!(
            world.create(world.owner, HandleId(0), actions(&["run"])).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        assert!(
            !world.outputs.iter().any(|o| matches!(o, Output::Event(SecurityEvent::EndpointCreated { .. })))
        );
    }
}

#[test]
fn object_action_bounds_and_ordinary_delegation_cannot_be_widened() {
    let mut world = World::new(config(), factory());
    assert_eq!(
        world.create(world.owner, HandleId(0), BTreeSet::new()).unwrap_err().code,
        ErrorCode::InvalidInput
    );
    let excessive =
        (0..=MAX_ENDPOINT_ACTIONS).map(|i| ActionName::new(format!("action-{i}")).unwrap()).collect();
    assert_eq!(world.create(world.owner, HandleId(0), excessive).unwrap_err().code, ErrorCode::InvalidInput);
    let created = world.allocate();
    let mut wide = grant(&created.invoke, ResourcePath::root());
    wide.actions.insert(ActionName::new("other").unwrap());
    assert_eq!(world.spawn(vec![wide]).unwrap_err().code, ErrorCode::PermissionDenied);
    let mut wide = interface(&created.endpoint);
    wide.actions.insert(ActionName::new("other").unwrap(), ActionDecl::internal(EffectClass::Observation));
    assert_eq!(
        world
            .rpc(world.owner, |request| ToKernel::Provide {
                request,
                authority: created.provide.handle,
                interface: wide
            })
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert!(world.kernel.provider_of(&created.endpoint).is_none());
    world.publish(world.owner, created.provide.handle, &created.endpoint).unwrap();
    world.rebind(world.owner, created.invoke.handle).unwrap();
    let denied = world.call(world.owner, created.invoke.handle, "other", ResourcePath::root());
    assert_eq!(world.response(denied).unwrap_err().code, ErrorCode::PermissionDenied);
}

#[test]
fn broad_publish_and_guessed_invocation_handles_cannot_adopt_an_owned_object() {
    let mut world = World::new(config(), factory());
    let created = world.allocate();
    let attacker = world
        .kernel
        .boot_process(
            ProgramName::new("agent").unwrap(),
            vec![spec("kernel", &["provide"], &["endpoint"]), spec(created.endpoint.as_str(), &["run"], &[])],
            0,
        )
        .unwrap();
    world.kernel.drain_outputs();
    assert_eq!(
        world.publish(attacker, HandleId(0), &created.endpoint).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        world.publish(attacker, HandleId(0), &EndpointName::new("dynamic.99999").unwrap()).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    world.child_provider(&created);
    assert_eq!(world.rebind(attacker, HandleId(1)).unwrap_err().code, ErrorCode::PermissionDenied);
    let denied = world.call(attacker, HandleId(1), "run", ResourcePath::root());
    assert_eq!(world.response(denied).unwrap_err().code, ErrorCode::PermissionDenied);
    assert_eq!(
        world.rpc(attacker, |request| ToKernel::Tools { request }).unwrap(),
        ResponseBody::Tools(vec![])
    );
}

#[test]
fn quota_and_invalid_requests_are_atomic_and_do_not_charge_the_factory() {
    let mut config = config();
    config.max_handles_per_process = 4;
    let mut capped = factory();
    capped.max_calls = Some(2);
    let mut world = World::new(config, capped);
    assert!(world.create(world.owner, HandleId(0), BTreeSet::new()).is_err());
    let first = world.allocate();
    assert_eq!(
        world.create(world.owner, HandleId(0), actions(&["run"])).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    for handle in [first.provide.handle, first.invoke.handle] {
        world.rpc(world.owner, |request| ToKernel::Close { request, handle }).unwrap();
    }
    let second = world.allocate();
    assert_ne!(first.endpoint, second.endpoint);
    assert_eq!(second.endpoint.as_str(), "dynamic.2", "failed allocations do not reserve identities");
    assert_eq!(second.provide.handle, HandleId(4), "handle identities are never reused");
}

#[test]
fn creation_publication_and_invocations_share_the_factory_budget() {
    let mut capped = factory();
    capped.max_calls = Some(3);
    let mut world = World::new(config(), capped);
    let created = world.allocate(); // one
    world.child_provider(&created); // two: provide; spawn has its own capability
    let admitted = world.call(world.owner, created.invoke.handle, "run", ResourcePath::root()); // three
    assert!(!world.outputs.iter().any(|o| matches!(o, Output::Send { msg: FromKernel::Response { request, .. }, .. } if *request == admitted)));
    let denied = world.call(world.owner, created.invoke.handle, "run", ResourcePath::root());
    assert_eq!(world.response(denied).unwrap_err().code, ErrorCode::BudgetExhausted);
    assert_eq!(
        world.create(world.owner, HandleId(0), actions(&["run"])).unwrap_err().code,
        ErrorCode::BudgetExhausted
    );
}

#[test]
fn factory_expiry_prevents_dispatch_and_removes_the_publication() {
    let mut expiring = factory();
    expiring.ttl_ms = Some(10);
    let mut world = World::new(config(), expiring);
    let created = world.allocate();
    assert_eq!(created.provide.expires_in_ms, Some(10));
    assert_eq!(created.invoke.expires_in_ms, Some(10));
    world.child_provider(&created);
    world.now = 10;
    let denied = world.call(world.owner, created.invoke.handle, "run", ResourcePath::root());
    assert_eq!(world.response(denied).unwrap_err().code, ErrorCode::Revoked);
    assert!(world.kernel.provider_of(&created.endpoint).is_none());
    assert!(
        !world.outputs.iter().any(|o| matches!(o, Output::Send { msg: FromKernel::Dispatch { .. }, .. }))
    );
    assert_eq!(
        world.create(world.owner, HandleId(0), actions(&["run"])).unwrap_err().code,
        ErrorCode::Revoked
    );
}

#[test]
fn closing_either_original_handle_or_factory_releases_the_endpoint_without_reuse() {
    for which in 0..3 {
        let mut world = World::new(config(), factory());
        let created = world.allocate();
        world.child_provider(&created);
        let handle = [created.provide.handle, created.invoke.handle, HandleId(0)][which];
        world.rpc(world.owner, |request| ToKernel::Close { request, handle }).unwrap();
        assert!(world.kernel.provider_of(&created.endpoint).is_none());
        assert!(world.outputs.iter().any(|o| matches!(o, Output::Event(SecurityEvent::EndpointReleased { endpoint, .. }) if *endpoint == created.endpoint)));
        let attacker = world
            .kernel
            .boot_process(
                ProgramName::new("agent").unwrap(),
                vec![spec("kernel", &["provide"], &["endpoint"])],
                0,
            )
            .unwrap();
        assert_eq!(
            world.publish(attacker, HandleId(0), &created.endpoint).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        if which < 2 {
            assert_ne!(world.allocate().endpoint, created.endpoint);
        }
    }
}

#[test]
fn an_owner_exit_cleans_its_objects_and_children_without_touching_other_owners() {
    let mut world = World::new(config(), factory());
    let created = world.allocate();
    let child = world.child_provider(&created);
    let other = world.kernel.boot_process(ProgramName::new("agent").unwrap(), vec![factory()], 0).unwrap();
    let independent = world.create(other, HandleId(0), actions(&["run"])).unwrap();
    world.publish(other, independent.provide.handle, &independent.endpoint).unwrap();
    world.kernel.process_exited(world.owner, 0);
    assert!(!world.kernel.is_live(child));
    assert!(world.kernel.provider_of(&created.endpoint).is_none());
    assert!(world.kernel.provider_of(&independent.endpoint).is_some());
}

#[test]
fn a_delegated_factory_keeps_parent_expiry_budget_and_depth_limits() {
    let mut limits = config();
    limits.max_delegation_depth = 1;
    let mut world = World::new(limits, factory());
    let delegated_factory = Grant {
        from: HandleId(0),
        actions: actions(&["create_endpoint"]),
        resource: path(&["endpoint"]),
        ttl_ms: None,
        delegable: true,
        max_calls: None,
    };
    let child = world.spawn(vec![delegated_factory]).unwrap();
    assert_eq!(
        world.create(child, HandleId(0), actions(&["run"])).unwrap_err().code,
        ErrorCode::LimitExceeded
    );

    let mut factory = factory();
    factory.max_calls = Some(2);
    factory.ttl_ms = Some(10);
    let mut world = World::new(config(), factory);
    let child = world
        .spawn(vec![Grant {
            from: HandleId(0),
            actions: actions(&["create_endpoint"]),
            resource: path(&["endpoint"]),
            ttl_ms: Some(100),
            delegable: true,
            max_calls: Some(100),
        }])
        .unwrap();
    let created = world.create(child, HandleId(0), actions(&["run"])).unwrap();
    assert_eq!(created.invoke.expires_in_ms, Some(10));
    world.publish(child, created.provide.handle, &created.endpoint).unwrap();
    assert_eq!(
        world.create(world.owner, HandleId(0), actions(&["run"])).unwrap_err().code,
        ErrorCode::BudgetExhausted
    );
}
