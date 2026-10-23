//! Agent-authored plugins, exercised through the real chat loop and confined
//! children. All model replies are scripted; no credentials or paid APIs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::http::{FakeHttp, Scripted, completion, tool_request};
use nucleus_e2e::{Scenario, TIMEOUT, UiClient, cap, endpoint, program};
use nucleus_kernel::SecurityEvent;
use nucleus_platform::{Bind, Supervisor};
use nucleus_protocol::ProcessId;
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";

/// This source is delivered by a model fs_create call, not installed as a
/// trusted service. It speaks only the public framed protocol using stdlib.
fn provider_source(version: &str) -> String {
    r#"import json, struct, sys
VERSION = VERSION_LITERAL
def receive():
    header = sys.stdin.buffer.read(4)
    if not header:
        return None
    count = struct.unpack('>I', header)[0]
    return json.loads(sys.stdin.buffer.read(count))
hello = receive()
wire_version = hello['v']
identity = hello['msg']['hello']
endpoint = sys.argv[1]
authority = next(h['handle'] for h in identity['handles'] if h['endpoint'] == 'kernel' and 'provide' in h['actions'])
def send(message):
    body = json.dumps({'v': wire_version, 'msg': message}).encode()
    sys.stdout.buffer.write(struct.pack('>I', len(body)) + body)
    sys.stdout.buffer.flush()
send({'provide': {'request': 1, 'authority': authority, 'interface': {
    'endpoint': endpoint, 'version': 1,
    'description': 'Authored plugin version ' + VERSION,
    'actions': {'echo': {'effect': 'observation', 'tool': {
        'description': 'Echo from authored plugin ' + VERSION,
        'params': {'type': 'object', 'properties': {'text': {'type': 'string'}}, 'required': ['text']},
        'resource_from': 'handle'
    }}}
}}})
while True:
    envelope = receive()
    if envelope is None or envelope['msg'] == 'terminate':
        break
    message = envelope['msg']
    if 'response' in message and 'Err' in message['response']['result']:
        sys.exit(2)
    if 'dispatch' in message:
        call = message['dispatch']
        send({'reply': {'dispatch': call['dispatch'], 'result': {'Ok': {
            'version': VERSION, 'text': call['payload']['text'],
            'endpoint': endpoint, 'process': identity['process']
        }}}})
"#.replace("VERSION_LITERAL", &format!("{version:?}"))
}

struct World {
    ui: UiClient,
    supervisor: Supervisor,
    scenario: Scenario,
    model: FakeHttp,
    /// The sessions service.
    parent: ProcessId,
    /// The agent of session `root`, which owns the plugins it starts.
    agent: ProcessId,
}

fn boot() -> World {
    boot_in("local")
}

fn boot_in(env: &str) -> World {
    boot_with_network(env, true, false)
}

fn boot_with_network(env: &str, create_endpoints: bool, network: bool) -> World {
    let model = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.write("work/chat/.keep", "");
    scenario.add_fs("fs", "work", "fs");
    scenario.add_openrouter("model", &model.base_url(), "sk-test-authored-plugin", &["--retries", "0"], true);
    scenario.add_conversation("conversation");
    let bindings = json!([{
        "service":"fs", "endpoint":"fs", "env":env, "workspace":"chat", "resource":["chat"],
        "description":"Local author workspace shared with the explicit plugin namespace"
    }])
    .to_string();
    let context = json!({
        "env":env, "workspace":"chat", "workspace_dir":"/work/chat", "programs_dir":"/programs",
        "chat_executable":"/programs/nucleus-chat-agent", "mcp_executable":"/programs/nucleus-mcp",
        "python_executable":"/interpreters/python3", "network": network
    })
    .to_string();
    scenario.add_chat_agent(
        "chat",
        MODEL,
        &["--max-steps", "24", "--tool-bindings", &bindings, "--execution-context", &context],
    );
    scenario.bind("chat", "work/chat", "/work/chat", true);
    scenario.bind_programs("chat");
    scenario.add_sessions("parent", "chat");
    scenario.add_ui("ui");
    let mut config = scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64);
    for name in ["chat", "parent"] {
        config.programs.get_mut(&program(name)).unwrap().network = network;
    }
    if network {
        config
            .programs
            .get_mut(&program("parent"))
            .unwrap()
            .args
            .extend(["--execution-context".into(), context.clone()]);
    }
    for name in ["chat", "parent"] {
        config.programs.get_mut(&program(name)).unwrap().binds.push(Bind {
            host: std::fs::canonicalize("/usr/bin/python3").unwrap(),
            guest: "/interpreters/python3".into(),
            writable: false,
        });
    }
    // Each session's agent sees it too, from the sessions service's namespace.
    let parent = config.programs.get_mut(&program("parent")).unwrap();
    parent.args.extend(["--child-bind".to_owned(), "/interpreters/python3".to_owned()]);
    let supervisor = scenario.start_with(config).unwrap();
    for name in ["fs", "model", "conversation"] {
        scenario.boot_service(&supervisor, name, name);
    }
    let mut provide = cap("kernel", &["provide"], &["endpoint", "sessions"]);
    let mut clear = cap("conversation", &["clear_reminders", "list"], &[]);
    provide.delegable = false;
    clear.delegable = false;
    let mut grants = vec![
        provide,
        cap("kernel", &["spawn_executable"], &["executable"]),
        // As composed: the parent narrows reminders to each session's agent.
        cap("conversation", &["receive", "post", "history", "remind", "reminders", "cancel_reminder"], &[]),
        clear,
        cap("model", &["complete"], &["deepseek"]),
        cap("fs", &["read", "create", "append"], &["chat"]),
    ];
    if create_endpoints {
        grants.push(cap("kernel", &["create_endpoint"], &["endpoint"]));
    }
    let parent = supervisor.boot(program("parent"), grants).unwrap();
    assert!(supervisor.wait_until(TIMEOUT, |kernel| kernel.provider_of(&endpoint("sessions")).is_some()));
    supervisor
        .boot(
            program("ui"),
            vec![
                cap("conversation", &["say", "history"], &[]),
                cap("sessions", &["create", "list", "stop"], &[]),
            ],
        )
        .unwrap();
    let mut ui = scenario.connect_ui("ui");
    let root = ui.request(json!({"op":"sessions.create", "session":"root"})).unwrap();
    let agent = ProcessId(root["process"].as_u64().unwrap());
    World { ui, supervisor, scenario, model, parent, agent }
}

fn start_args(name: &str, source: &str) -> Value {
    json!({"name":name, "env":"local", "actions":["echo"], "grants":[], "program":{
        "executable":"/interpreters/python3", "args":[format!("/work/chat/{source}"), "{endpoint}"],
        "binds":[{"path":"/work/chat", "writable":false}], "network":false
    }})
}

#[test]
fn mcp_connect_publishes_tools_reuses_connection_and_stops_by_name() {
    let remote = FakeHttp::start();
    remote.script_with("\"method\":\"initialize\"", |received| {
        let request: Value = serde_json::from_str(&received.body).unwrap();
        Scripted::json(
            200,
            &json!({"jsonrpc":"2.0","id":request["id"],"result":{
            "protocolVersion":"2025-03-26","capabilities":{"tools":{}},
            "serverInfo":{"name":"fixture","version":"1"}}}),
        )
    });
    remote.script_for("notifications/initialized", Scripted::json(202, &json!(null)));
    remote.script_with("tools/list", |received| {
        let request: Value = serde_json::from_str(&received.body).unwrap();
        Scripted::json(200, &json!({"jsonrpc":"2.0","id":request["id"],"result":{"tools":[{
            "name":"echo","description":"Echo a value","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
        }]}}))
    });
    remote.script_with("tools/call", |received| {
        let request: Value = serde_json::from_str(&received.body).unwrap();
        assert_eq!(request["params"]["arguments"]["text"], "hello");
        Scripted::json(200, &json!({"jsonrpc":"2.0","id":request["id"],"result":{"content":[{"type":"text","text":"hello"}]}}))
    });
    let mut world = boot_with_network("local", true, true);
    let connect = json!({"name":"remote","url":remote.base_url(),"tools":["echo"]});
    script(&world, &[("connect", "mcp_connect", connect.clone())]);
    script(&world, &[("again", "mcp_connect", connect.clone())]);
    script(&world, &[("echo", "plugin-remote_echo", json!({"env":"local","text":"hello"}))]);
    script(&world, &[("inspect", "plugin_capabilities", json!({"describe":"remote"}))]);
    let mut conflict = connect.clone();
    conflict["tools"] = json!(["different"]);
    script(&world, &[("conflict", "mcp_connect", conflict)]);
    script(&world, &[("stop", "plugin_stop", json!({"name":"remote"}))]);
    finish(&world);
    world.ui.say("root", "connect", "Connect, use, and disconnect the remote service.").unwrap();
    let log = world.ui.wait_turns("root", 1);
    assert_eq!(success(&log, "connect")["tools"], json!(["plugin-remote_echo"]));
    assert_eq!(success(&log, "again")["state"], "connected");
    assert!(has_tool(&request(&world, 1), "plugin-remote_echo"));
    assert!(success(&log, "echo").to_string().contains("hello"));
    assert_eq!(success(&log, "inspect")["actions"][0]["action"], "echo");
    assert!(serde_json::to_string(&log).unwrap().contains("different configuration"));
    success(&log, "stop");
    assert_eq!(remote.received().iter().filter(|r| r.body.contains("tools/list")).count(), 1);
}

#[test]
fn mcp_connect_cannot_add_network_authority_and_cleans_failed_start() {
    let remote = FakeHttp::start();
    let mut world = boot();
    script(
        &world,
        &[("connect", "mcp_connect", json!({"name":"remote","url":remote.base_url(),"tools":["echo"]}))],
    );
    script(&world, &[("list", "plugin_list", json!({}))]);
    finish(&world);
    world.ui.say("root", "connect", "Try connecting without network access.").unwrap();
    let log = world.ui.wait_turns("root", 1);
    assert_eq!(entry(&log, "connect")["error_code"], "permission_denied");
    assert_eq!(success(&log, "list")["plugins"], json!([]));
    assert!(remote.received().is_empty());
    assert!(!has_tool(&request(&world, 1), "plugin-remote_echo"));
}

fn script(world: &World, calls: &[(&str, &str, Value)]) {
    world.model.script(Scripted::json(200, &tool_request(MODEL, calls)));
}

fn finish(world: &World) {
    world.model.script(Scripted::json(200, &completion(MODEL, "fixture done")));
}

fn entry<'a>(log: &'a [Value], id: &str) -> &'a Value {
    log.iter()
        .find(|entry| entry["kind"] == "tool_result" && entry["call_id"] == id)
        .unwrap_or_else(|| panic!("missing result {id}: {log:?}"))
}

fn success(log: &[Value], id: &str) -> Value {
    let entry = entry(log, id);
    assert!(entry["error_code"].is_null(), "{id}: {entry}");
    serde_json::from_str(entry["summary"].as_str().unwrap()).unwrap()
}

fn request(world: &World, index: usize) -> Value {
    serde_json::from_str(&world.model.received()[index].body).unwrap()
}

fn has_tool(request: &Value, name: &str) -> bool {
    request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == name)
}

#[test]
fn main_chat_authors_starts_replaces_and_stops_a_plugin_within_one_message() {
    exercise_plugin_lifecycle(boot(), "local");
}

#[test]
fn ordinary_programs_stop_and_restart_without_endpoint_authority_or_inherited_grants() {
    let mut world = boot_with_network("local", false, false);
    let source = r#"import json, struct, sys
def receive():
    header = sys.stdin.buffer.read(4)
    if not header:
        return None
    return json.loads(sys.stdin.buffer.read(struct.unpack('>I', header)[0]))['msg']
identity = receive()['hello']
with open(sys.argv[1], 'w') as file:
    json.dump(identity, file)
while True:
    message = receive()
    if message is None or message == 'terminate':
        break
"#;
    let args = |version: &str| {
        json!({
            "name":"client", "env":"local", "program":{
                "executable":"/interpreters/python3",
                "args":[format!("/work/chat/client-{version}.py"), format!("/work/chat/identity-{version}.json")],
                "binds":[{"path":"/work/chat", "writable":true}]
            }
        })
    };
    let mut first = args("one");
    first["grants"] = json!([{"endpoint":"fs", "actions":["read"], "resource":["chat"]}]);
    script(
        &world,
        &[(
            "write-one",
            "fs_create",
            json!({"env":"local", "workspace":"chat", "path":"client-one.py", "content":source}),
        )],
    );
    script(&world, &[("first", "plugin_start", first)]);
    finish(&world);
    world
        .ui
        .say("root", "plain-one", "Write and start an ordinary client without publishing any tools.")
        .unwrap();
    let log = world.ui.wait_turns("root", 1);
    let first = success(&log, "first");
    assert!(first["endpoint"].is_null());
    assert!(first["service"].is_null());
    assert!(
        world.supervisor.wait_until(TIMEOUT, |_| world.scenario.host("work/chat/identity-one.json").exists())
    );

    let mut second = args("two");
    second["actions"] = json!([]);
    script(&world, &[("stop-one", "plugin_stop", json!({"name":"client"}))]);
    script(
        &world,
        &[(
            "write-two",
            "fs_create",
            json!({"env":"local", "workspace":"chat", "path":"client-two.py", "content":source}),
        )],
    );
    script(&world, &[("second", "plugin_start", second)]);
    finish(&world);
    world
        .ui
        .say(
            "root",
            "plain-two",
            "Stop the client, prepare its replacement, and restart with no service grants.",
        )
        .unwrap();
    let log = world.ui.wait_turns("root", 2);
    assert_eq!(success(&log, "stop-one")["state"], "stopped");
    let second = success(&log, "second");
    assert_ne!(first["process"], second["process"]);
    assert!(
        world.supervisor.wait_until(TIMEOUT, |_| world.scenario.host("work/chat/identity-two.json").exists())
    );
    script(
        &world,
        &[
            ("stop-two", "plugin_stop", json!({"name":"client"})),
            ("stop-again", "plugin_stop", json!({"name":"client"})),
            ("final-list", "plugin_list", json!({})),
        ],
    );
    finish(&world);
    world.ui.say("root", "plain-stop", "Stop the restarted client and check cleanup.").unwrap();
    let log = world.ui.wait_turns("root", 3);
    for id in ["stop-two", "stop-again"] {
        assert_eq!(success(&log, id)["state"], "stopped");
    }
    assert_eq!(success(&log, "final-list")["plugins"][0]["state"], "stopped");
    let identity = |version: &str| -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(world.scenario.host(&format!("work/chat/identity-{version}.json")))
                .unwrap(),
        )
        .unwrap()
    };
    let first_identity = identity("one");
    assert_eq!(first_identity["handles"].as_array().unwrap().len(), 1);
    assert_eq!(first_identity["handles"][0]["endpoint"], "fs");
    assert_eq!(first_identity["handles"][0]["actions"], json!(["read"]));
    assert_eq!(identity("two")["handles"], json!([]), "stopped instance grants are never resurrected");
    assert!(
        !world.scenario.events().iter().any(
            |event| matches!(event, SecurityEvent::EndpointCreated { owner, .. } if *owner == world.agent)
        )
    );
    for started in [&first, &second] {
        assert!(!world.supervisor.is_live(ProcessId(started["process"].as_u64().unwrap())));
    }
    assert!(
        std::fs::read_to_string(world.scenario.host("work/chat/client-one.py")).is_ok(),
        "stopping retains source files"
    );
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn replacement_waits_for_an_independently_supervised_grandchild_to_finish_cleanup() {
    let mut world = boot();
    let grandchild = r#"import json, struct, sys, time
def receive():
    header = sys.stdin.buffer.read(4)
    if not header:
        return None
    return json.loads(sys.stdin.buffer.read(struct.unpack('>I', header)[0]))['msg']
identity = receive()['hello']
with open('/work/chat/grandchild.ready', 'w') as file:
    file.write(str(identity['process']))
while True:
    message = receive()
    if message is None:
        sys.exit(2)
    if message == 'terminate':
        time.sleep(0.2)
        with open('/work/chat/grandchild.cleaned', 'w') as file:
            file.write('cleanup finished')
        break
"#;
    let spawn = r#"spawn_authority = next(h['handle'] for h in identity['handles'] if h['endpoint'] == 'kernel' and 'spawn_executable' in h['actions'])
send({'spawn_executable': {'request': 2, 'authority': spawn_authority, 'program': {
    'executable': '/interpreters/python3', 'args': ['/work/chat/grandchild.py'],
    'binds': [{'path': '/work/chat', 'writable': True}], 'network': False
}, 'grants': []}})
response = receive()['msg']['response']['result']['Ok']
grandchild = response['spawned']['child']
deadline = time.monotonic() + 2
while not os.path.exists('/work/chat/grandchild.ready'):
    if time.monotonic() >= deadline:
        sys.exit(3)
    time.sleep(0.005)
"#;
    let parent = provider_source("parent")
        .replace("import json, struct, sys", "import json, struct, sys, os, time")
        .replace("send({'provide':", &format!("{spawn}\nsend({{'provide':"))
        .replace(
            "'process': identity['process']",
            "'process': identity['process'], 'grandchild': grandchild",
        );
    world.scenario.write("work/chat/parent.py", &parent);
    world.scenario.write("work/chat/grandchild.py", grandchild);
    let replacement = provider_source("replacement")
        .replace("import json, struct, sys", "import json, struct, sys, os")
        .replace(
            "send({'provide':",
            "if not os.path.exists('/work/chat/grandchild.cleaned'):\n    sys.exit(9)\nsend({'provide':",
        );
    world.scenario.write("work/chat/replacement.py", &replacement);
    let mut args = start_args("tree", "parent.py");
    args["program"]["binds"] = json!([
        {"path":"/work/chat", "writable":true}, {"path":"/interpreters/python3"}
    ]);
    args["grants"] = json!([{
        "endpoint":"kernel", "actions":["spawn_executable"], "resource":["executable", "interpreters", "python3"]
    }]);
    script(&world, &[("start", "plugin_start", args)]);
    script(&world, &[("call", "plugin-tree_echo", json!({"env":"local", "text":"identify"}))]);
    script(&world, &[("replace", "plugin_replace", start_args("tree", "replacement.py"))]);
    script(
        &world,
        &[("cleaned", "fs_read", json!({"env":"local", "workspace":"chat", "path":"grandchild.cleaned"}))],
    );
    script(&world, &[("stop", "plugin_stop", json!({"name":"tree"}))]);
    finish(&world);
    world.ui.say("root", "tree", "Replace the plugin only after its descendant finishes cleanup.").unwrap();
    let log = world.ui.wait_turns("root", 1);
    success(&log, "start");
    let original = success(&log, "call");
    success(&log, "replace");
    assert_eq!(success(&log, "cleaned")["content"], "cleanup finished");
    success(&log, "stop");
    let parent = ProcessId(original["process"].as_u64().unwrap());
    let child = ProcessId(original["grandchild"].as_u64().unwrap());
    assert!(world.scenario.children_of(parent).contains(&child));
    assert!(world.supervisor.wait_until(TIMEOUT, |kernel| !kernel.is_live(child)));
    assert!(world.supervisor.shutdown().is_clean());
}

fn exercise_plugin_lifecycle(mut world: World, env: &str) {
    let start = |source: &str| {
        let mut args = start_args("echo", source);
        args["env"] = json!(env);
        args
    };
    script(
        &world,
        &[(
            "write-one",
            "fs_create",
            json!({"env":env, "workspace":"chat", "path":"v1.py", "content":provider_source("one")}),
        )],
    );
    script(&world, &[("start", "plugin_start", start("v1.py"))]);
    script(&world, &[("call-one", "plugin-echo_echo", json!({"env":env, "text":"first"}))]);
    script(
        &world,
        &[(
            "write-two",
            "fs_create",
            json!({"env":env, "workspace":"chat", "path":"v2.py", "content":provider_source("two")}),
        )],
    );
    script(&world, &[("replace", "plugin_replace", start("v2.py"))]);
    script(&world, &[("call-two", "plugin-echo_echo", json!({"env":env, "text":"second"}))]);
    script(&world, &[("list-live", "plugin_list", json!({}))]);
    script(&world, &[("stop", "plugin_stop", json!({"name":"echo"}))]);
    script(&world, &[("list-stopped", "plugin_list", json!({}))]);
    finish(&world);
    world.ui.say("root", "u1", "Write and exercise the two plugin versions.").unwrap();
    let log = world.ui.wait_turns("root", 1);
    for id in ["write-one", "start", "write-two", "replace", "list-live", "stop", "list-stopped"] {
        success(&log, id);
    }
    assert_eq!(success(&log, "list-live")["plugins"][0]["state"], "running");
    assert_eq!(success(&log, "list-live")["scope"], "created_by_current_agent");
    assert_eq!(success(&log, "stop")["state"], "stopped");
    assert_eq!(success(&log, "list-stopped")["plugins"][0]["state"], "stopped");
    let first = success(&log, "call-one");
    let second = success(&log, "call-two");
    assert_eq!((&first["version"], &first["text"]), (&json!("one"), &json!("first")));
    assert_eq!((&second["version"], &second["text"]), (&json!("two"), &json!("second")));
    assert!(first["endpoint"].as_str().unwrap().starts_with("dynamic."));
    assert_ne!(first["endpoint"], second["endpoint"], "replacement gets a fresh endpoint");
    assert_ne!(first["process"], second["process"]);
    assert!(!has_tool(&request(&world, 0), "plugin-echo_echo"));
    assert!(has_tool(&request(&world, 2), "plugin-echo_echo"), "published before the next model step");
    assert!(request(&world, 2).to_string().contains("Authored plugin version one"));
    assert!(request(&world, 5).to_string().contains("Authored plugin version two"));
    assert!(!has_tool(&request(&world, 8), "plugin-echo_echo"), "stop withdraws the same-turn catalog entry");
    for result in [&first, &second] {
        let process = ProcessId(result["process"].as_u64().unwrap());
        assert!(world.supervisor.wait_until(TIMEOUT, |kernel| !kernel.is_live(process)));
        assert!(world.scenario.children_of(world.agent).contains(&process));
        assert!(world.supervisor.provider_of(&endpoint(result["endpoint"].as_str().unwrap())).is_none());
    }
    assert_eq!(
        std::fs::read_to_string(world.scenario.host("work/chat/v2.py")).unwrap(),
        provider_source("two")
    );
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn plugin_programs_cannot_widen_namespace_network_or_delegated_capabilities() {
    let mut world = boot();
    world.scenario.write("work/chat/provider.py", &provider_source("survivor"));
    // Tool `plugin-a_b_c`, which a plugin `a_b` with action `c` would name again.
    world.scenario.write("work/chat/first.py", &provider_source("first").replace("'echo':", "'b_c':"));
    world.scenario.write("work/chat/collision.py", &provider_source("collision").replace("'echo':", "'c':"));
    let mut first = start_args("a", "first.py");
    first["actions"] = json!(["b_c"]);
    script(
        &world,
        &[
            ("capabilities", "plugin_capabilities", json!({})),
            ("start", "plugin_start", start_args("echo", "provider.py")),
            ("first", "plugin_start", first),
        ],
    );
    finish(&world);
    world.ui.say("root", "u1", "Start the permitted plugins.").unwrap();
    let log = world.ui.wait_turns("root", 1);
    success(&log, "start");
    success(&log, "first");
    let capabilities = success(&log, "capabilities");
    assert_eq!(capabilities["execution_context"]["env"], "local");
    assert!(capabilities["grants"].as_array().unwrap().iter().all(|grant| grant.get("handle").is_none()));
    let before = world
        .scenario
        .events()
        .iter()
        .filter(|event| matches!(event, SecurityEvent::ProcessStarted { .. }))
        .count();
    let mut filesystem = start_args("outside", "provider.py");
    filesystem["program"]["binds"] = json!([{"path":"/etc", "writable":false}]);
    let mut workspace_parent = start_args("workspace-parent", "provider.py");
    workspace_parent["program"]["binds"] = json!([{"path":"/work", "writable":true}]);
    let mut network = start_args("network", "provider.py");
    network["program"]["network"] = json!(true);
    let mut delegation = start_args("delegation", "provider.py");
    delegation["grants"] = json!([{"endpoint":"fs", "actions":["read"], "resource":[], "delegable":false}]);
    // Held delegable on every session, but only for each session's own agent.
    let mut reminders = start_args("reminders", "provider.py");
    reminders["grants"] =
        json!([{"endpoint":"conversation", "actions":["remind", "cancel_reminder"], "resource":["work"]}]);
    // Held there only because the parent is the sessions service.
    let mut other_session = start_args("other-session", "provider.py");
    other_session["grants"] =
        json!([{"endpoint":"conversation", "actions":["receive", "post"], "resource":["work"]}]);
    let mut executable = start_args("executable", "provider.py");
    executable["program"]["executable"] = json!("/usr/bin/python3");
    let mut replace = start_args("echo", "provider.py");
    replace["program"]["network"] = json!(true);
    let mut foreign_env = start_args("foreign-env", "provider.py");
    foreign_env["env"] = json!("compute");
    script(
        &world,
        &[
            ("outside", "plugin_start", filesystem),
            ("workspace-parent", "plugin_start", workspace_parent),
            ("network", "plugin_start", network),
            ("delegation", "plugin_start", delegation),
            ("reminders", "plugin_start", reminders),
            ("other-session", "plugin_start", other_session),
            ("executable", "plugin_start", executable),
            ("foreign-env", "plugin_start", foreign_env),
            ("survivor", "plugin-echo_echo", json!({"env":"local", "text":"still running"})),
            ("failed-replace", "plugin_replace", replace),
            ("after-replace", "plugin_list", json!({})),
        ],
    );
    finish(&world);
    world.ui.say("root", "u2", "Try starts beyond the explicitly inherited authority.").unwrap();
    let log = world.ui.wait_turns("root", 2);
    for id in [
        "outside",
        "workspace-parent",
        "network",
        "delegation",
        "reminders",
        "other-session",
        "executable",
        "foreign-env",
        "failed-replace",
    ] {
        assert_eq!(entry(&log, id)["error_code"], "permission_denied", "{id}: {log:?}");
    }
    assert_eq!(success(&log, "survivor")["version"], "survivor");
    let after = world
        .scenario
        .events()
        .iter()
        .filter(|event| matches!(event, SecurityEvent::ProcessStarted { .. }))
        .count();
    assert_eq!(before, after, "none of the forbidden programs starts a process");
    let listed = success(&log, "after-replace");
    let echo = listed["plugins"].as_array().unwrap().iter().find(|p| p["name"] == "echo").unwrap();
    assert_eq!(
        echo["state"], "stopped",
        "replacement stops its old instance before attempting the confined spawn"
    );
    // A start whose tool name collides with a running plugin's is refused once
    // its tools are seen, and the running plugin keeps its name.
    let mut collision = start_args("a_b", "collision.py");
    collision["actions"] = json!(["c"]);
    script(
        &world,
        &[
            ("restart", "plugin_start", start_args("echo", "provider.py")),
            ("collision", "plugin_start", collision),
        ],
    );
    script(
        &world,
        &[
            ("restarted", "plugin-echo_echo", json!({"env":"local", "text":"explicit restart"})),
            ("still-first", "plugin-a_b_c", json!({"env":"local", "text":"original route"})),
        ],
    );
    finish(&world);
    world.ui.say("root", "u3", "Restart the stopped plugin with its permitted configuration.").unwrap();
    let log = world.ui.wait_turns("root", 3);
    success(&log, "restart");
    assert_eq!(success(&log, "restarted")["version"], "survivor");
    assert_ne!(success(&log, "survivor")["process"], success(&log, "restarted")["process"]);
    assert_eq!(entry(&log, "collision")["error_code"], "invalid_input", "{log:?}");
    assert_eq!(success(&log, "still-first")["version"], "first");
    let last = request(&world, world.model.received().len() - 1);
    let named = last["tools"].as_array().unwrap().iter().filter(|t| t["function"]["name"] == "plugin-a_b_c");
    assert_eq!(named.count(), 1, "a refused plugin cannot poison later tool catalogs");
    assert!(world.supervisor.shutdown().is_clean(), "shutdown owns the restarted plugin");
}

#[test]
fn replacement_failure_leaves_the_name_stopped_until_an_explicit_fresh_start() {
    let mut world = boot();
    world.scenario.write("work/chat/good.py", &provider_source("original"));
    world.scenario.write("work/chat/crash.py", "import sys\nsys.exit(7)\n");
    world.scenario.write(
        "work/chat/not-ready.py",
        r#"import json, struct, sys
while True:
    header = sys.stdin.buffer.read(4)
    if not header:
        break
    body = sys.stdin.buffer.read(struct.unpack('>I', header)[0])
    if json.loads(body)['msg'] == 'terminate':
        break
"#,
    );
    script(&world, &[("start", "plugin_start", start_args("echo", "good.py"))]);
    script(&world, &[("before", "plugin-echo_echo", json!({"env":"local", "text":"original provider"}))]);
    finish(&world);
    world.ui.say("root", "u1", "Start the original plugin.").unwrap();
    let log = world.ui.wait_turns("root", 1);
    success(&log, "start");
    let mut before = success(&log, "before");
    for (index, (id, source, timeout, code)) in
        [("crash", "crash.py", 1000, "unavailable"), ("timeout", "not-ready.py", 100, "timeout")]
            .into_iter()
            .enumerate()
    {
        let mut candidate = start_args("echo", source);
        candidate["ready_timeout_ms"] = json!(timeout);
        let stopped_id = format!("{id}-stopped");
        let unavailable_id = format!("{id}-old-call");
        let restart_id = format!("{id}-restart");
        let call_id = format!("{id}-new-call");
        script(&world, &[(id, "plugin_replace", candidate)]);
        script(
            &world,
            &[
                (&stopped_id, "plugin_list", json!({})),
                (
                    &unavailable_id,
                    "plugin-echo_echo",
                    json!({"env":"local", "text":"must not use the old process"}),
                ),
                (&restart_id, "plugin_start", start_args("echo", "good.py")),
            ],
        );
        script(&world, &[(&call_id, "plugin-echo_echo", json!({"env":"local", "text":"explicit restart"}))]);
        finish(&world);
        world
            .ui
            .say(
                "root",
                &format!("u{}", index + 2),
                "A failed replacement must stay stopped until restarted.",
            )
            .unwrap();
        let log = world.ui.wait_turns("root", index + 2);
        assert_eq!(entry(&log, id)["error_code"], code);
        assert_eq!(entry(&log, &unavailable_id)["error_code"], "invalid_input");
        let plugins = success(&log, &stopped_id);
        assert_eq!(plugins["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(plugins["plugins"][0]["state"], "stopped");
        success(&log, &restart_id);
        let after = success(&log, &call_id);
        assert_eq!(after["version"], "original");
        assert_ne!(before["process"], after["process"]);
        assert_ne!(before["endpoint"], after["endpoint"]);
        assert!(!world.supervisor.is_live(ProcessId(before["process"].as_u64().unwrap())));
        assert!(world.supervisor.provider_of(&endpoint(before["endpoint"].as_str().unwrap())).is_none());
        before = after;
    }
    let children = world.scenario.children_of(world.agent);
    assert_eq!(children.len(), 5, "two failed candidates and two explicit restarts really started");
    let current = ProcessId(before["process"].as_u64().unwrap());
    for child in children.into_iter().filter(|child| *child != current) {
        assert!(world.supervisor.wait_until(TIMEOUT, |kernel| !kernel.is_live(child)));
    }
    assert_eq!(
        std::fs::read_to_string(world.scenario.host("work/chat/good.py")).unwrap(),
        provider_source("original")
    );
    assert!(world.supervisor.shutdown().is_clean());
}

/// A plugin belongs to the session's agent that started it: another session
/// can neither call nor stop it. Stopping a session ends its agent and the
/// whole tree of plugins it started; the sessions service, the other sessions
/// and their plugins go on, and the session's next agent starts none of the
/// old ones.
#[test]
fn stopping_a_session_reaps_its_plugin_tree_without_stopping_other_sessions() {
    let mut world = boot();
    let named = world.ui.request(json!({"op":"sessions.create", "session":"other"})).unwrap();
    let named_child = ProcessId(named["process"].as_u64().unwrap());
    world.scenario.write("work/chat/other.py", &provider_source("other"));
    script(&world, &[("other-start", "plugin_start", start_args("other", "other.py"))]);
    finish(&world);
    world.ui.say("other", "other-start", "Start an independently owned plugin.").unwrap();
    let log = world.ui.wait_turns("other", 1);
    let other = success(&log, "other-start");
    let other_plugin = ProcessId(other["process"].as_u64().unwrap());
    assert!(world.scenario.children_of(named_child).contains(&other_plugin));
    assert!(!world.scenario.children_of(world.agent).contains(&other_plugin));

    // This is a real kernel-supervised descendant, not an OS-only child. Its
    // delayed termination marker must already exist when own Stop returns.
    let leaf = r#"import json, struct, sys, time
def receive():
    header = sys.stdin.buffer.read(4)
    if not header:
        return None
    return json.loads(sys.stdin.buffer.read(struct.unpack('>I', header)[0]))['msg']
identity = receive()['hello']
with open('/work/chat/stop-leaf.ready', 'w') as file:
    file.write(str(identity['process']))
while True:
    message = receive()
    if message is None:
        sys.exit(2)
    if message == 'terminate':
        time.sleep(0.2)
        with open('/work/chat/stop-leaf.cleaned', 'w') as file:
            file.write('cleaned')
        break
"#;
    let spawn = r#"spawn_authority = next(h['handle'] for h in identity['handles'] if h['endpoint'] == 'kernel' and 'spawn_executable' in h['actions'])
send({'spawn_executable': {'request': 2, 'authority': spawn_authority, 'program': {
    'executable': '/interpreters/python3', 'args': ['/work/chat/stop-leaf.py'],
    'binds': [{'path': '/work/chat', 'writable': True}], 'network': False
}, 'grants': []}})
grandchild = receive()['msg']['response']['result']['Ok']['spawned']['child']
"#;
    let tree =
        provider_source("root").replace("send({'provide':", &format!("{spawn}\nsend({{'provide':")).replace(
            "'process': identity['process']",
            "'process': identity['process'], 'grandchild': grandchild",
        );
    world.scenario.write("work/chat/stop-tree.py", &tree);
    world.scenario.write("work/chat/stop-leaf.py", leaf);
    let mut args = start_args("owned", "stop-tree.py");
    args["program"]["binds"] = json!([
        {"path":"/work/chat", "writable":true}, {"path":"/interpreters/python3"}
    ]);
    args["grants"] = json!([{
        "endpoint":"kernel", "actions":["spawn_executable"], "resource":["executable", "interpreters", "python3"]
    }]);
    script(
        &world,
        &[
            ("owned-start", "plugin_start", args),
            ("foreign", "plugin-other_echo", json!({"env":"local", "text":"must not reach another owner"})),
            ("foreign-stop", "plugin_stop", json!({"name":"other"})),
        ],
    );
    script(&world, &[("owned-identity", "plugin-owned_echo", json!({"env":"local", "text":"identify"}))]);
    finish(&world);
    let root_request = world.model.received().len();
    world.ui.say("root", "owned-start", "Start a plugin with a supervised descendant.").unwrap();
    let log = world.ui.wait_turns("root", 1);
    assert!(!has_tool(&request(&world, root_request), "plugin-other_echo"));
    assert_eq!(entry(&log, "foreign")["error_code"], "invalid_input");
    assert_eq!(entry(&log, "foreign-stop")["error_code"], "invalid_input");
    let original = success(&log, "owned-identity");
    let owned = ProcessId(original["process"].as_u64().unwrap());
    let leaf = ProcessId(original["grandchild"].as_u64().unwrap());
    assert!(world.scenario.children_of(owned).contains(&leaf));
    assert!(nucleus_e2e::wait_for(TIMEOUT, || world.scenario.host("work/chat/stop-leaf.ready").is_file()));

    world.ui.request(json!({"op":"sessions.stop", "session":"root"})).unwrap();
    assert!(nucleus_e2e::wait_for(TIMEOUT, || {
        std::fs::read_to_string(world.scenario.host("work/chat/stop-leaf.cleaned"))
            .is_ok_and(|t| t == "cleaned")
    }));
    assert!(world.supervisor.wait_until(TIMEOUT, |k| !k.is_live(owned) && !k.is_live(leaf)));
    assert!(!world.supervisor.is_live(world.agent));
    assert!(world.supervisor.provider_of(&endpoint(original["endpoint"].as_str().unwrap())).is_none());
    assert!(world.supervisor.is_live(world.parent));
    assert!(world.supervisor.is_live(named_child));
    assert!(world.supervisor.is_live(other_plugin));
    assert_eq!(std::fs::read_to_string(world.scenario.host("work/chat/stop-tree.py")).unwrap(), tree);

    script(&world, &[("other-still-live", "plugin-other_echo", json!({"env":"local", "text":"unaffected"}))]);
    finish(&world);
    world.ui.say("other", "other-again", "Use the independently owned plugin after root Stop.").unwrap();
    let log = world.ui.wait_turns("other", 2);
    assert_eq!(success(&log, "other-still-live")["process"], other_plugin.0);

    let resumed = world.ui.request(json!({"op":"sessions.create", "session":"root"})).unwrap();
    assert_ne!(resumed["process"], world.agent.0, "a stopped session's next agent is a new process");
    script(&world, &[("after-stop-list", "plugin_list", json!({}))]);
    script(&world, &[("restart-owned", "plugin_start", start_args("owned", "other.py"))]);
    finish(&world);
    let resume_request = world.model.received().len();
    world
        .ui
        .say("root", "root-resume", "List stopped plugins, then explicitly start a fresh instance.")
        .unwrap();
    // Stopped while idle, so Stop added no turn end: this is the second turn.
    let log = world.ui.wait_turns("root", 2);
    assert_eq!(success(&log, "after-stop-list")["plugins"], json!([]), "a new agent owns no plugin yet");
    assert!(!has_tool(&request(&world, resume_request), "plugin-owned_echo"));
    let restarted = success(&log, "restart-owned");
    assert_ne!(restarted["process"], owned.0);
    assert_ne!(restarted["endpoint"], original["endpoint"]);

    // The other session's Stop reaps its agent and its plugin in turn.
    world.ui.request(json!({"op":"sessions.stop", "session":"other"})).unwrap();
    assert!(world.supervisor.wait_until(TIMEOUT, |k| !k.is_live(named_child) && !k.is_live(other_plugin)));
    assert!(world.supervisor.provider_of(&endpoint(other["endpoint"].as_str().unwrap())).is_none());
    assert!(world.supervisor.is_live(world.parent));
    let report = world.supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
    for process in [owned, leaf, other_plugin] {
        assert!(
            report.reaped.iter().any(|entry| entry.process == process && entry.error.is_none()),
            "{report:?}"
        );
    }
}

#[test]
fn stopping_during_plugin_readiness_cleans_the_child_and_never_executes_the_rest_of_the_batch() {
    let mut world = boot();
    let source = r#"import json, struct, sys, time
def receive():
    header = sys.stdin.buffer.read(4)
    if not header:
        return None
    return json.loads(sys.stdin.buffer.read(struct.unpack('>I', header)[0]))['msg']
identity = receive()['hello']
with open('/work/chat/starting-plugin.json', 'w') as file:
    json.dump(identity, file)
# Deliberately never publish the requested endpoint: Stop interrupts readiness.
while True:
    message = receive()
    if message is None:
        sys.exit(2)
    if message == 'terminate':
        time.sleep(0.2)
        with open('/work/chat/starting-plugin.cleaned', 'w') as file:
            file.write('cleaned')
        break
"#;
    world.scenario.write("work/chat/starting.py", source);
    let mut args = start_args("starting", "starting.py");
    args["ready_timeout_ms"] = json!(30_000);
    args["program"]["binds"][0]["writable"] = json!(true);
    script(
        &world,
        &[
            ("starting", "plugin_start", args),
            (
                "must-not-run",
                "fs_create",
                json!({"env":"local", "workspace":"chat", "path":"unexpected.txt", "content":"must not execute"}),
            ),
        ],
    );
    world.ui.say("root", "start-pending", "Start a slow plugin, then write the file.").unwrap();
    let identity = world.scenario.host("work/chat/starting-plugin.json");
    assert!(nucleus_e2e::wait_for(TIMEOUT, || std::fs::read_to_string(&identity)
        .ok()
        .is_some_and(|text| serde_json::from_str::<Value>(&text).is_ok())));
    let identity: Value = serde_json::from_str(&std::fs::read_to_string(identity).unwrap()).unwrap();
    let child = ProcessId(identity["process"].as_u64().unwrap());
    let started = std::time::Instant::now();
    world.ui.request(json!({"op":"sessions.stop", "session":"root"})).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "Stop waited for the 30-second readiness timeout"
    );
    assert!(nucleus_e2e::wait_for(TIMEOUT, || {
        std::fs::read_to_string(world.scenario.host("work/chat/starting-plugin.cleaned"))
            .is_ok_and(|t| t == "cleaned")
    }));
    assert!(world.supervisor.wait_until(TIMEOUT, |k| !k.is_live(child)));
    assert!(!world.scenario.host("work/chat/unexpected.txt").exists());
    assert_eq!(world.model.received().len(), 1, "Stop must not ask the model to replay the batch");
    assert!(world.supervisor.is_live(world.parent));
    assert_eq!(std::fs::read_to_string(world.scenario.host("work/chat/starting.py")).unwrap(), source);

    // The session's next agent starts from the conversation alone: nothing
    // of the stopped batch is asked of the model again, and no plugin starts.
    finish(&world);
    let resumed = world.ui.request(json!({"op":"sessions.create", "session":"root"})).unwrap();
    let agent = ProcessId(resumed["process"].as_u64().unwrap());
    world.ui.say("root", "resume-pending", "Resume with this new instruction.").unwrap();
    world.ui.wait_turns("root", 2);
    let later = &world.model.received()[1..];
    assert!(!later.is_empty() && later.iter().all(|r| !r.body.contains("must-not-run")), "{later:?}");
    assert!(!world.scenario.host("work/chat/unexpected.txt").exists());
    assert!(world.scenario.children_of(agent).is_empty(), "the stopped plugin is not recreated");
    let report = world.supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
    assert!(report.reaped.iter().any(|entry| entry.process == child && entry.error.is_none()), "{report:?}");
}
