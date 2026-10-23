//! Environment routing through the real chat loop, kernel and confined providers.
//! Two environments are two sets of ordinary providers; routing never merges them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::http::{FakeHttp, Scripted, completion, tool_request};
use nucleus_e2e::{Scenario, UiClient, binaries, cap, program};
use nucleus_platform::{Bind, ProgramSpec, Supervisor};
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";

struct World {
    // Drop the live runtime before removing its bind mounts on a failed test.
    ui: UiClient,
    supervisor: Supervisor,
    scenario: Scenario,
    model: FakeHttp,
}

fn boot() -> World {
    let model = FakeHttp::start();
    let mut scenario = Scenario::new();
    let mut bindings = Vec::new();
    for env in ["local", "compute"] {
        scenario.write(&format!("{env}/chat/seed.txt"), env);
        scenario.add_fs(&format!("{env}-fs"), env, &format!("{env}-fs"));
        for service in ["fs", "shell"] {
            bindings.push(json!({
                "service": service, "endpoint": format!("{env}-{service}"),
                "env": env, "workspace": "chat", "resource": ["chat"],
                "description": format!("{env} isolated chat workspace; fs and shell share files")
            }));
        }
    }
    // Declaring a route does not authorize its endpoint. This provider exists,
    // but the chat agent is given no capability on it.
    scenario.write("restricted/chat/seed.txt", "not granted");
    scenario.add_fs("restricted-fs", "restricted", "restricted-fs");
    bindings.push(json!({
        "service": "fs", "endpoint": "restricted-fs", "env": "restricted",
        "workspace": "chat", "resource": ["chat"], "description": "not granted"
    }));
    scenario.add_openrouter(
        "model",
        &model.base_url(),
        "sk-test-tool-environments",
        &["--retries", "0"],
        true,
    );
    scenario.add_conversation("conversation");
    scenario.add_chat_agent("chat", MODEL, &["--tool-bindings", &json!(bindings).to_string()]);
    scenario.add_ui("ui");
    let mut config = scenario.config(|_| {}, 64);
    for env in ["local", "compute"] {
        config.programs.insert(
            program(&format!("{env}-shell")),
            ProgramSpec {
                executable: binaries().shell.clone(),
                args: vec![
                    "--root".into(),
                    "/work".into(),
                    "--endpoint".into(),
                    format!("{env}-shell"),
                    "--env".into(),
                    "PATH=/usr/bin:/bin".into(),
                    "--description".into(),
                    format!("{env} endpoint environment marker"),
                ],
                binds: vec![Bind { host: scenario.host(env), guest: "/work".into(), writable: true }],
                network: false,
            },
        );
    }
    let supervisor = scenario.start_with(config).unwrap();
    for name in
        ["local-fs", "local-shell", "compute-fs", "compute-shell", "restricted-fs", "model", "conversation"]
    {
        scenario.boot_service(&supervisor, name, name);
    }
    let mut grants = vec![
        cap("model", &["complete"], &["deepseek"]),
        cap("conversation", &["receive", "post", "history"], &["default", "default"]),
    ];
    for env in ["local", "compute"] {
        grants.push(cap(&format!("{env}-fs"), &["read", "create"], &["chat"]));
        grants.push(cap(&format!("{env}-shell"), &["exec"], &["chat"]));
    }
    supervisor.boot(program("chat"), grants).unwrap();
    supervisor
        .boot(program("ui"), vec![cap("conversation", &["say", "history"], &["default", "default"])])
        .unwrap();
    let ui = scenario.connect_ui("ui");
    World { scenario, model, supervisor, ui }
}

fn script_turn(world: &World, calls: &[(&str, &str, Value)]) {
    world.model.script(Scripted::json(200, &tool_request(MODEL, calls)));
    world.model.script(Scripted::json(200, &completion(MODEL, "done")));
}

fn result<'a>(log: &'a [Value], id: &str) -> &'a Value {
    log.iter()
        .find(|entry| entry["kind"] == "tool_result" && entry["call_id"] == id)
        .unwrap_or_else(|| panic!("missing result for {id}: {log:?}"))
}

fn sent_tools(world: &World, request: usize) -> Vec<Value> {
    let body: Value = serde_json::from_str(&world.model.received()[request].body).unwrap();
    body["tools"].as_array().unwrap().iter().map(|tool| tool["function"].clone()).collect()
}

#[test]
fn fs_and_shell_share_the_selected_workspace_and_keep_environments_separate() {
    let mut world = boot();
    script_turn(
        &world,
        &[
            (
                "local-write",
                "fs_create",
                json!({"env":"local", "workspace":"chat", "path":"same.txt", "content":"local data"}),
            ),
            (
                "compute-write",
                "fs_create",
                json!({"env":"compute", "workspace":"chat", "path":"same.txt", "content":"compute data"}),
            ),
            (
                "compute-shell",
                "shell_exec",
                json!({"env":"compute", "workspace":"chat", "command":"cat same.txt; printf 'shell result' > shell.txt"}),
            ),
            (
                "local-shell",
                "shell_exec",
                json!({"env":"local", "workspace":"chat", "command":"cat same.txt"}),
            ),
            ("compute-read", "fs_read", json!({"env":"compute", "workspace":"chat", "path":"shell.txt"})),
            ("local-read", "fs_read", json!({"env":"local", "workspace":"chat", "path":"same.txt"})),
        ],
    );
    world.ui.say("default", "u1", "Use each explicitly selected environment.").unwrap();
    let log = world.ui.wait_turns("default", 1);
    for id in ["local-write", "compute-write", "compute-shell", "local-shell", "compute-read", "local-read"] {
        assert!(result(&log, id)["error_code"].is_null(), "{id}: {log:?}");
    }
    assert!(result(&log, "compute-shell")["summary"].as_str().unwrap().contains("compute data"));
    assert!(result(&log, "local-shell")["summary"].as_str().unwrap().contains("local data"));
    assert!(result(&log, "compute-read")["summary"].as_str().unwrap().contains("shell result"));
    assert_eq!(std::fs::read_to_string(world.scenario.host("local/chat/same.txt")).unwrap(), "local data");
    assert_eq!(
        std::fs::read_to_string(world.scenario.host("compute/chat/same.txt")).unwrap(),
        "compute data"
    );
    assert!(!world.scenario.host("local/chat/shell.txt").exists());

    let tools = sent_tools(&world, 0);
    for name in ["fs_create", "fs_read", "shell_exec"] {
        let matching: Vec<_> = tools.iter().filter(|tool| tool["name"] == name).collect();
        assert_eq!(matching.len(), 1, "one common tool per action: {tools:?}");
        let schema = &matching[0]["parameters"];
        assert!(schema["required"].as_array().unwrap().contains(&json!("env")), "{schema}");
        assert!(schema["required"].as_array().unwrap().contains(&json!("workspace")), "{schema}");
        assert!(schema.to_string().contains("local") && schema.to_string().contains("compute"), "{schema}");
    }
    let request: Value = serde_json::from_str(&world.model.received()[0].body).unwrap();
    let context = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str())
        .find(|content| content.contains("local endpoint environment marker"))
        .expect("the provider catalog reaches the model");
    assert!(context.contains("local endpoint environment marker"), "{context}");
    assert!(context.contains("compute endpoint environment marker"), "{context}");
    assert!(
        context.contains("Commands run with `bash -e -o pipefail -c`"),
        "the shell provider's environment description reaches the model: {context}"
    );
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn route_names_and_model_arguments_cannot_grant_an_environment_or_escape_a_workspace() {
    let mut world = boot();
    script_turn(
        &world,
        &[
            (
                "restricted",
                "fs_create",
                json!({"env":"restricted", "workspace":"chat", "path":"forbidden.txt", "content":"no"}),
            ),
            (
                "unknown",
                "fs_create",
                json!({"env":"invented", "workspace":"chat", "path":"forbidden.txt", "content":"no"}),
            ),
            (
                "workspace",
                "shell_exec",
                json!({"env":"compute", "workspace":"private", "command":"touch forbidden.txt"}),
            ),
            (
                "traversal",
                "fs_create",
                json!({"env":"compute", "workspace":"chat", "path":"../forbidden.txt", "content":"no"}),
            ),
            ("allowed", "fs_read", json!({"env":"compute", "workspace":"chat", "path":"seed.txt"})),
        ],
    );
    world.ui.say("default", "u1", "Try the recorded tool selections.").unwrap();
    let log = world.ui.wait_turns("default", 1);
    for id in ["restricted", "unknown", "workspace", "traversal"] {
        assert!(result(&log, id)["error_code"].is_string(), "{id}: {log:?}");
    }
    assert!(result(&log, "allowed")["error_code"].is_null(), "{log:?}");
    assert!(world.scenario.access_log("restricted").is_empty(), "no call reaches the ungranted provider");
    assert!(world.scenario.access_log("local").is_empty(), "no default local routing of invalid calls");
    assert_eq!(world.scenario.access_log("compute").len(), 1, "only the permitted read is dispatched");
    for file in [
        "restricted/chat/forbidden.txt",
        "local/chat/forbidden.txt",
        "compute/forbidden.txt",
        "compute/private/forbidden.txt",
    ] {
        assert!(!world.scenario.host(file).exists(), "{file}");
    }
    let tools = sent_tools(&world, 0);
    assert!(!json!(tools).to_string().contains("restricted"), "ungranted route absent from model schema");
    assert!(world.supervisor.shutdown().is_clean());
}
