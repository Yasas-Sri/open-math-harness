//! The GUI integration of the discovery service is a grant, not code: `sessions`
//! holds a delegable `discovery {start, status, stop, list, problems, tree}`,
//! forwards it verbatim to every chat agent it starts, and the agent turns the
//! actions into tools through the kernel's `tools` query — so the model is
//! offered `discovery_start`, `discovery_status`, `discovery_stop`,
//! `discovery_list`, `discovery_problems` and `discovery_tree` with the
//! interface's schemas. Real confined processes, a scripted model, no Lean: the
//! resident service needs only its authority and its namespace to provide its
//! endpoints; the Lean service is consulted when a task is started, which
//! nothing here does.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use nucleus_e2e::http::{FakeHttp, Scripted, completion};
use nucleus_e2e::{DiscoveryService, Scenario, TIMEOUT, cap, discovery_grants, endpoint, program};
use nucleus_kernel::CapSpec;
use serde_json::{Value, json};

/// A `tree` reply fits one transport frame, with room for the envelope around it.
#[test]
fn the_tree_reply_bound_fits_one_frame() {
    let (tree, frame) = (nucleus_discovery_interface::MAX_TREE_BYTES, nucleus_protocol::MAX_FRAME_BYTES);
    assert!(tree + 32 * 1024 <= frame, "{tree} under {frame}");
}

const MODEL: &str = "deepseek/deepseek-v4-flash";
const KEY: &str = "sk-or-test-DISCOVERY-TOOLS-0123456789";

/// The sessions service's authority: what every agent may be given, delegable —
/// the conversation, the model, and the discovery service.
fn sessions_grants() -> Vec<CapSpec> {
    let mut provide = cap("kernel", &["provide"], &["endpoint", "sessions"]);
    let mut catalog = cap("conversation", &["list"], &[]);
    provide.delegable = false;
    catalog.delegable = false;
    vec![
        provide,
        nucleus_e2e::session_spawn(),
        catalog,
        cap("conversation", &["receive", "post", "history"], &[]),
        cap("model", &["complete", "start"], &["deepseek"]),
        cap("discovery", &["start", "status", "stop", "list", "problems", "tree"], &[]),
    ]
}

/// A front end's authority: speak as the user, read history, manage sessions.
fn ui_grants() -> Vec<CapSpec> {
    let mut grants = vec![
        cap("conversation", &["say", "history"], &[]),
        cap("sessions", &["create", "stop", "list"], &[]),
    ];
    for grant in &mut grants {
        grant.delegable = false;
    }
    grants
}

#[test]
fn a_chat_agent_holding_the_forwarded_capability_is_offered_the_six_discovery_tools() {
    let server = FakeHttp::start();
    server.script(Scripted::json(200, &completion(MODEL, "Nothing to run yet.")));

    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    scenario.add_conversation("conversation");
    scenario.add_chat_agent("chat", MODEL, &["--max-steps", "2"]);
    scenario.add_sessions("sessions", "chat");
    scenario.add_ui("ui");
    let service = DiscoveryService {
        tasks: scenario.host("tasks"),
        policies: scenario.host("policies"),
        prover_args: vec!["--model".to_owned(), MODEL.to_owned()],
        // No run is started here; the service still insists that the absence
        // of a USD ceiling be declared rather than defaulted into.
        extra: vec!["--no-usd-ceiling".to_owned()],
    };
    scenario.add_discovery("discovery", &service);

    // Parked `receive` calls occupy in-flight slots of the conversation endpoint.
    let mut supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    scenario.boot_service(&supervisor, "openrouter", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    // The discovery service holds its `lean` and `explore` grants against
    // endpoints nobody provides yet: it needs them only once a task is started.
    let discovery = supervisor.boot(program("discovery"), discovery_grants(4)).unwrap();
    for name in ["discovery", "explore"] {
        let target = endpoint(name);
        assert!(
            supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == discovery)),
            "the service provides `{name}`"
        );
    }
    let sessions = supervisor.boot(program("sessions"), sessions_grants()).unwrap();
    let target = endpoint("sessions");
    assert!(supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == sessions)));
    supervisor.boot(program("ui"), ui_grants()).unwrap();
    let mut ui = scenario.connect_ui("ui");

    // Starting a session forwards the capability verbatim: same actions, same
    // (root) resource, beside the conversation and the model.
    let created = ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let granted: Vec<String> = created["grants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| format!("{} {} {}", g["endpoint"].as_str().unwrap(), g["resource"], g["actions"]))
        .collect();
    assert_eq!(
        granted,
        [
            r#"conversation ["default","default"] ["post","receive"]"#,
            r#"conversation ["default","default"] ["history"]"#,
            r#"model ["deepseek"] ["complete","start"]"#,
            r#"discovery [] ["list","problems","start","status","stop","tree"]"#
        ]
    );

    // The agent's first model call carries its tool listing: the six
    // discovery actions, named `<endpoint>_<action>`, with the schemas the
    // interface declares. Nothing else is callable in this composition.
    ui.say("default", "u1", "Hello.").unwrap();
    let log = ui.wait_turns("default", 1);
    assert!(log.iter().any(|r| r["kind"] == "assistant" && r["text"] == "Nothing to run yet."), "{log:?}");
    let received = server.received();
    assert_eq!(received.len(), 1, "one model call");
    let body: Value = serde_json::from_str(&received[0].body).unwrap();
    let tools = body["tools"].as_array().expect("the request offers tools");
    let names: BTreeSet<&str> = tools.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "discovery_list",
            "discovery_problems",
            "discovery_start",
            "discovery_status",
            "discovery_stop",
            "discovery_tree",
        ]),
        "{body}"
    );
    let function = |name: &str| -> Value {
        tools.iter().find(|t| t["function"]["name"] == name).map(|t| t["function"].clone()).unwrap()
    };
    let start = function("discovery_start");
    assert_eq!(start["parameters"]["required"], json!(["v", "name", "concurrency"]), "{start}");
    for field in ["problems", "problems_file", "prompt_template", "prompt_template_file", "policy", "limits"]
    {
        assert!(start["parameters"]["properties"].get(field).is_some(), "{field} in the task schema");
    }
    assert!(start["description"].as_str().unwrap().contains("Spends model budget"), "{start}");
    for name in ["discovery_status", "discovery_stop", "discovery_problems"] {
        let tool = function(name);
        assert_eq!(tool["parameters"]["required"], json!(["run"]), "{name} names the run: {tool}");
    }
    assert_eq!(function("discovery_list")["parameters"]["required"], Value::Null);
    // A two-segment resource is one `path` argument, `<run>/<problem>`.
    let tree = function("discovery_tree");
    assert_eq!(tree["parameters"]["required"], json!(["path"]), "{tree}");
    assert!(
        tree["parameters"]["properties"]["path"]["description"].as_str().unwrap().contains("<run>/<problem>"),
        "{tree}"
    );

    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}
