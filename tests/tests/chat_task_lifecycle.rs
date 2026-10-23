//! A real one-shot chat client must wait for its authored plugin before exiting.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nucleus_e2e::http::{FakeHttp, Scripted, completion, tool_request};
use nucleus_e2e::{Scenario, TIMEOUT, binaries, cap, program};
use nucleus_platform::{Bind, ProgramSpec};
use nucleus_protocol::ProcessId;
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";
const FRAMING: &str = r#"import json, os, struct, sys, time
def receive():
    prefix = sys.stdin.buffer.read(4)
    if not prefix:
        raise RuntimeError('unexpected kernel EOF')
    return json.loads(sys.stdin.buffer.read(struct.unpack('>I', prefix)[0]))
hello = receive()
version = hello['v']
identity = hello['msg']['hello']
def send(message):
    body = json.dumps({'v': version, 'msg': message}).encode()
    sys.stdout.buffer.write(struct.pack('>I', len(body)) + body)
    sys.stdout.buffer.flush()
def publish(path, value):
    with open(path + '.next', 'w') as file:
        json.dump(value, file)
    os.replace(path + '.next', path)
"#;

fn json_file(path: &Path) -> Option<Value> {
    fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Locate only this fixture's real executable, not its bubblewrap wrapper or
/// another concurrently running chat test. PID namespaces hide host IDs from
/// the child, so this observation belongs to the test's host-side operator.
fn task_host_processes(task: &str) -> Vec<u32> {
    let executable = fs::metadata(&binaries().chat).unwrap();
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let candidate = fs::metadata(entry.path().join("exe")).ok()?;
            // A bind mount can give the same executable a different pathname
            // in the child's namespace; compare the actual backing file.
            if (candidate.dev(), candidate.ino()) != (executable.dev(), executable.ino()) {
                return None;
            }
            let command = fs::read(entry.path().join("cmdline")).ok()?;
            command.split(|byte| *byte == 0).any(|argument| argument == task.as_bytes()).then_some(pid)
        })
        .collect()
}

#[test]
fn one_shot_chat_remains_alive_until_its_owned_plugin_cleans_up() {
    let model = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.write("work/chat/.keep", "");
    scenario.add_openrouter(
        "model",
        &model.base_url(),
        "synthetic-task-lifecycle-key",
        &["--retries", "0"],
        true,
    );
    let plugin = format!(
        "{FRAMING}{}",
        r#"
endpoint = sys.argv[1]
authority = next(h['handle'] for h in identity['handles'] if 'provide' in h['actions'])
send({'provide': {'request': 1, 'authority': authority, 'interface': {
    'endpoint': endpoint, 'version': 1, 'description': 'Owned lifecycle fixture',
    'actions': {'ready': {'effect': 'observation', 'tool': {
        'description': 'Confirm initialization', 'params': {'type': 'object', 'properties': {}},
        'resource_from': 'handle'
    }}}
}}})
while True:
    message = receive()['msg']
    if message == 'terminate':
        publish('/work/chat/cleanup-started.json', {'process': identity['process']})
        deadline = time.monotonic() + 8
        while not os.path.exists('/work/chat/release-cleanup'):
            if time.monotonic() >= deadline:
                raise RuntimeError('test never released cleanup barrier')
            time.sleep(0.005)
        publish('/work/chat/plugin-cleaned.json', {'process': identity['process']})
        break
    if 'response' in message and 'Err' in message['response']['result']:
        raise RuntimeError('endpoint publication was rejected')
    if 'dispatch' in message:
        send({'reply': {'dispatch': message['dispatch']['dispatch'], 'result': {'Ok': {'ready': True}}}})
"#
    );
    scenario.write("work/chat/plugin.py", &plugin);
    let task = format!("Finish this isolated lifecycle task {}", scenario.host("identity").display());
    let context = json!({
        "env":"local", "workspace":"chat", "workspace_dir":"/work/chat", "programs_dir":"/programs",
        "chat_executable":"/programs/nucleus-chat-agent", "mcp_executable":"/programs/nucleus-mcp",
        "python_executable":"/interpreters/python3"
    });
    let child_program = json!({
        "executable":"/programs/nucleus-chat-agent",
        "args":["--model",MODEL,"--max-steps","4","--task",task,"--execution-context",context.to_string()],
        "binds":[{"path":"/work/chat","writable":true},{"path":"/programs"},{"path":"/interpreters/python3"}]
    });
    let observer = format!(
        "{FRAMING}\nrequest_program = json.loads({:?})\n{}",
        child_program.to_string(),
        r#"
spawn = next(h['handle'] for h in identity['handles'] if h['endpoint'] == 'kernel' and 'spawn_executable' in h['actions'])
grants = [{'from': h['handle'], 'actions': h['actions'], 'resource': h['resource'],
    'delegable': h['delegable'], 'ttl_ms': None, 'max_calls': None} for h in identity['handles']]
send({'spawn_executable': {'request': 1, 'authority': spawn, 'program': request_program, 'grants': grants}})
task = receive()['msg']['response']['result']['Ok']['spawned']['child']
publish('/work/chat/task-started.json', {'process': task})
while True:
    message = receive()['msg']
    if 'child_reaped' in message and message['child_reaped']['child'] == task:
        assert os.path.exists('/work/chat/plugin-cleaned.json')
        publish('/work/chat/task-reaped.json', message['child_reaped'])
    if message == 'terminate':
        break
"#
    );
    scenario.write("work/chat/observer.py", &observer);
    let mut config = scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64);
    config.grace = std::time::Duration::from_secs(10);
    let python = fs::canonicalize("/usr/bin/python3").unwrap();
    config.programs.insert(
        program("observer"),
        ProgramSpec {
            executable: python.clone(),
            args: vec!["/work/chat/observer.py".into()],
            network: false,
            binds: vec![
                Bind { host: scenario.host("work/chat"), guest: "/work/chat".into(), writable: true },
                Bind {
                    host: binaries().chat.parent().unwrap().to_owned(),
                    guest: "/programs".into(),
                    writable: false,
                },
                Bind { host: python, guest: "/interpreters/python3".into(), writable: false },
            ],
        },
    );
    model.script(Scripted::json(
        200,
        &tool_request(
            MODEL,
            &[(
                "start",
                "plugin_start",
                json!({
                    "name":"owned", "env":"local", "actions":["ready"], "program":{
                        "executable":"/interpreters/python3", "args":["/work/chat/plugin.py","{endpoint}"],
                        "binds":[{"path":"/work/chat","writable":true}]
                    }
                }),
            )],
        ),
    ));
    model.script(Scripted::json(
        200,
        &tool_request(MODEL, &[("ready", "plugin-owned_ready", json!({"env":"local"}))]),
    ));
    model.script(Scripted::json(
        200,
        &completion(MODEL, "The task is complete; the owned program is still running."),
    ));
    let mut supervisor = scenario.start_with(config).unwrap();
    scenario.boot_service(&supervisor, "model", "model");
    let owner = supervisor
        .boot(
            program("observer"),
            vec![
                cap("kernel", &["spawn_executable"], &["executable"]),
                cap("kernel", &["create_endpoint"], &["endpoint"]),
                cap("model", &["complete"], &["deepseek"]),
            ],
        )
        .unwrap();
    assert!(
        supervisor
            .wait_until(TIMEOUT, |_| json_file(&scenario.host("work/chat/cleanup-started.json")).is_some()),
        "the task never began owned-plugin cleanup"
    );
    let task_process = ProcessId(
        json_file(&scenario.host("work/chat/task-started.json")).unwrap()["process"].as_u64().unwrap(),
    );
    let alive = task_host_processes(&task);
    // Always release the barrier before assertions so a failed regression does
    // not leave a test-created process waiting until its grace deadline.
    fs::write(scenario.host("work/chat/release-cleanup"), "release").unwrap();
    assert_eq!(alive.len(), 1, "the one-shot chat host exited before its owned plugin finished cleanup");
    assert!(
        supervisor.wait_until(TIMEOUT, |_| json_file(&scenario.host("work/chat/task-reaped.json")).is_some())
    );
    let receipt = json_file(&scenario.host("work/chat/task-reaped.json")).unwrap();
    assert_eq!(receipt["success"], true, "{receipt}");
    assert_eq!(receipt["cleanup_confirmed"], true, "{receipt}");
    assert!(supervisor.cleanup_confirmed(task_process));
    assert!(supervisor.is_live(owner));
    assert_eq!(scenario.children_of(owner), vec![task_process]);
    assert!(task_host_processes(&task).is_empty());
    assert_eq!(model.received().len(), 3);
    assert!(supervisor.shutdown().is_clean());
}

/// A one-shot task whose answer is still cut at the output cap after its last
/// allowed continuation ends as a failed child, cleaned up, in one turn.
#[test]
fn one_shot_length_exhaustion_is_a_failed_child() {
    let model = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.add_openrouter(
        "model",
        &model.base_url(),
        "synthetic-length-task-key",
        &["--retries", "0"],
        true,
    );
    scenario.add_conversation("conversation");
    scenario.add_chat_agent(
        "task",
        MODEL,
        &["--task", "Produce a complete answer.", "--max-length-continuations", "1"],
    );
    let observer = format!(
        "{FRAMING}{}",
        r#"
spawn = next(h['handle'] for h in identity['handles'] if h['endpoint'] == 'kernel' and 'spawn' in h['actions'])
grants = [{'from': h['handle'], 'actions': h['actions'], 'resource': h['resource'],
    'delegable': h['delegable'], 'ttl_ms': None, 'max_calls': None}
    for h in identity['handles'] if h['endpoint'] != 'kernel']
send({'spawn': {'request': 1, 'authority': spawn, 'program': 'task', 'grants': grants}})
task = receive()['msg']['response']['result']['Ok']['spawned']['child']
while True:
    message = receive()['msg']
    if 'child_reaped' in message and message['child_reaped']['child'] == task:
        publish('/out/reaped.json', message['child_reaped'])
    if message == 'terminate':
        break
"#
    );
    scenario.write("observer/observer.py", &observer);
    let mut config = scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64);
    config.programs.insert(
        program("observer"),
        ProgramSpec {
            executable: fs::canonicalize("/usr/bin/python3").unwrap(),
            args: vec!["/out/observer.py".into()],
            network: false,
            binds: vec![Bind { host: scenario.host("observer"), guest: "/out".into(), writable: true }],
        },
    );
    for _ in 0..2 {
        let mut reply = completion(MODEL, "The visible partial answer.");
        reply["choices"][0]["finish_reason"] = json!("length");
        model.script(Scripted::json(200, &reply));
    }
    let mut supervisor = scenario.start_with(config).unwrap();
    scenario.boot_service(&supervisor, "model", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let owner = supervisor
        .boot(
            program("observer"),
            vec![
                cap("kernel", &["spawn"], &["program", "task"]),
                cap("model", &["complete"], &["deepseek"]),
                cap("conversation", &["post"], &["default", "default"]),
            ],
        )
        .unwrap();
    assert!(supervisor.wait_until(TIMEOUT, |_| json_file(&scenario.host("observer/reaped.json")).is_some()));
    let receipt = json_file(&scenario.host("observer/reaped.json")).unwrap();
    assert_eq!(receipt["success"], false, "{receipt}");
    assert_eq!(receipt["cleanup_confirmed"], true, "{receipt}");
    assert!(supervisor.is_live(owner));
    assert_eq!(model.received().len(), 2, "the call and its one continuation");
    let log = scenario.conversation_log("default");
    assert_eq!(log.iter().filter(|r| r["kind"] == "turn_complete").count(), 1, "{log:?}");
    let errors: Vec<_> = log.iter().filter(|r| r["kind"] == "error").collect();
    assert_eq!(errors.len(), 1, "{log:?}");
    assert!(errors[0]["text"].as_str().unwrap().contains("continuation limit"));
    assert!(supervisor.shutdown().is_clean());
}

fn rejected_tool_record(reject_kind: &str) {
    let model = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.write("data/work/.keep", "");
    scenario.add_fs("fs", "data", "fs");
    scenario.add_openrouter(
        "model",
        &model.base_url(),
        "synthetic-post-rejection-key",
        &["--retries", "0"],
        true,
    );
    scenario.add_chat_agent(
        "task",
        MODEL,
        &["--task", "Create the two files once.", "--transcript", "work/retained.jsonl"],
    );
    let interface = nucleus_conversation_interface::interface(nucleus_e2e::endpoint("conversation")).unwrap();
    let interface_json = serde_json::to_string(&interface).unwrap();
    let observer = format!(
        "{FRAMING}\ninterface = json.loads({interface_json:?})\nreject_kind = {reject_kind:?}\n{}",
        r#"
provide = next(h['handle'] for h in identity['handles'] if h['endpoint'] == 'kernel' and 'provide' in h['actions'])
send({'provide': {'request': 1, 'authority': provide, 'interface': interface}})
assert 'Ok' in receive()['msg']['response']['result']
spawn = next(h['handle'] for h in identity['handles'] if h['endpoint'] == 'kernel' and 'spawn' in h['actions'])
grants = [{'from': h['handle'], 'actions': h['actions'], 'resource': h['resource'],
    'delegable': h['delegable'], 'ttl_ms': None, 'max_calls': None}
    for h in identity['handles'] if h['endpoint'] != 'kernel']
send({'spawn': {'request': 2, 'authority': spawn, 'program': 'task', 'grants': grants}})
records = []
task = None
while True:
    message = receive()['msg']
    if message == 'terminate':
        break
    if 'response' in message and message['response']['request'] == 2:
        task = message['response']['result']['Ok']['spawned']['child']
    if 'dispatch' in message:
        dispatch = message['dispatch']
        assert dispatch['action'] == 'post'
        entry = dispatch['payload']
        rejected = entry.get('kind') == reject_kind and entry.get('call_id') == 'first'
        records.append({'entry': entry, 'accepted': not rejected})
        publish('/out/conversation.json', records)
        result = {'Err': {'code': 'service_error', 'message': 'selected conversation record rejected'}} if rejected else {'Ok': {}}
        send({'reply': {'dispatch': dispatch['dispatch'], 'result': result}})
    if 'child_reaped' in message:
        assert task is not None and message['child_reaped']['child'] == task
        publish('/out/reaped.json', message['child_reaped'])
"#
    );
    scenario.write("observer/observer.py", &observer);
    let mut config = scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64);
    config.programs.insert(
        program("observer"),
        ProgramSpec {
            executable: fs::canonicalize("/usr/bin/python3").unwrap(),
            args: vec!["/out/observer.py".into()],
            network: false,
            binds: vec![Bind { host: scenario.host("observer"), guest: "/out".into(), writable: true }],
        },
    );
    model.script(Scripted::json(
        200,
        &tool_request(
            MODEL,
            &[
                ("first", "fs_create", json!({"path":"work/first.txt", "content":"FIRST EFFECT"})),
                ("second", "fs_create", json!({"path":"work/second.txt", "content":"SECOND EFFECT"})),
            ],
        ),
    ));
    let mut supervisor = scenario.start_with(config).unwrap();
    scenario.boot_service(&supervisor, "model", "model");
    scenario.boot_service(&supervisor, "fs", "fs");
    supervisor
        .boot(
            program("observer"),
            vec![
                cap("kernel", &["provide"], &["endpoint", "conversation"]),
                cap("kernel", &["spawn"], &["program", "task"]),
                cap("model", &["complete"], &["deepseek"]),
                cap("conversation", &["post"], &["default", "default"]),
                cap("fs", &["create", "append"], &["work"]),
            ],
        )
        .unwrap();
    assert!(supervisor.wait_until(TIMEOUT, |_| json_file(&scenario.host("observer/reaped.json")).is_some()));
    let reaped = json_file(&scenario.host("observer/reaped.json")).unwrap();
    assert_eq!(reaped["success"], false, "record rejection must fail the one-shot task: {reaped}");
    assert_eq!(reaped["cleanup_confirmed"], true);
    assert_eq!(model.received().len(), 1, "a failed recording must not continue the model loop");
    let first_executed = reject_kind == "tool_result";
    assert_eq!(scenario.host("data/work/first.txt").exists(), first_executed);
    if first_executed {
        assert_eq!(fs::read_to_string(scenario.host("data/work/first.txt")).unwrap(), "FIRST EFFECT");
    }
    assert!(!scenario.host("data/work/second.txt").exists());
    let access = scenario.access_log("data");
    assert_eq!(
        access.iter().filter(|entry| nucleus_e2e::mentions(entry, "first.txt")).count(),
        usize::from(first_executed)
    );
    assert!(!access.iter().any(|entry| nucleus_e2e::mentions(entry, "second.txt")));
    let records = json_file(&scenario.host("observer/conversation.json")).unwrap();
    let records = records.as_array().unwrap();
    assert_eq!(records.iter().filter(|r| r["accepted"] == false).count(), 1);
    assert!(records.iter().any(|r| r["accepted"] == false && r["entry"]["kind"] == reject_kind));
    assert!(records.iter().any(|r| r["accepted"] == true && r["entry"]["kind"] == "error"));
    assert_eq!(records.iter().filter(|r| r["entry"]["kind"] == "turn_complete").count(), 1);
    let history: Vec<Value> = fs::read_to_string(scenario.host("data/work/retained.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let calls: Vec<_> = history.iter().filter_map(|m| m["tool_calls"].as_array()).flatten().collect();
    assert_eq!(
        calls.iter().map(|call| call["id"].as_str().unwrap()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    let results: Vec<_> = history.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(
        results.iter().map(|m| m["tool_call_id"].as_str().unwrap()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    for (index, result) in results.iter().enumerate() {
        let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        if index == 0 && first_executed {
            assert_eq!(content, json!({}), "retain the actual successful filesystem result");
        } else {
            assert_eq!(content["error"]["code"], "service_error");
            assert!(content["error"]["message"].as_str().unwrap().contains("not executed"));
        }
    }
    assert!(supervisor.shutdown().is_clean());
}

#[test]
fn rejected_tool_call_record_prevents_every_side_effect_and_pairs_history() {
    rejected_tool_record("tool_call");
}

#[test]
fn rejected_tool_result_record_retains_the_completed_effect_and_stops_the_batch() {
    rejected_tool_record("tool_result");
}
