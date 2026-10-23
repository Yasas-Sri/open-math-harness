//! The prompt templates are checked against the tools that exist.
//!
//! A template that names a tool or a parameter the system does not have is
//! worse than an empty template: the model reads it, believes it, and calls
//! something that is not there.
//!
//! So the contract is mechanical: every identifier a template puts in
//! backticks must be a real tool name, a real parameter of one, or a term this
//! test knows about. Tools are what the kernel derives from a held capability,
//! `{endpoint}_{action}` (`nucleus_sdk::tool_name`), and only for actions whose
//! provider declared them tool-callable. Nothing here boots a service: each
//! interface is asked for its declaration.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nucleus_protocol::{EndpointName, InterfaceDecl};

/// Terms a template may use that are not tools: the assembly's own variables,
/// the configuration values they take, and the vocabulary of the system the
/// prompts describe. Anything not here and not a tool fails the test, which is
/// the point — the list is meant to be read and argued with.
const VOCABULARY: &[&str] = &[
    // The assembly's slots and the values they hold.
    "sandbox_mode",
    "approval_policy",
    "read-only",
    "workspace-write",
    "on-request",
    // The memory pipeline's own artifacts and fields. These are files it
    // maintains and JSON keys it must return, not tools.
    "session_summary",
    "session_slug",
    "memory_summary.md",
];

fn declarations() -> Vec<InterfaceDecl> {
    let name = |s: &str| EndpointName::new(s).expect("endpoint name");
    // The names a template may use do not depend on a service's ceilings.
    let ceilings = nucleus_discovery_interface::Ceilings {
        max_concurrency: 1,
        max_usd_micro_per_problem: 0,
        max_problems_per_task: 1,
        max_width: 1,
    };
    vec![
        nucleus_approvals_interface::interface(name("approvals")).expect("approvals"),
        nucleus_conversation_interface::interface(name("conversation")).expect("conversation"),
        nucleus_discovery_interface::interface(
            name("discovery"),
            &ceilings,
            nucleus_discovery_interface::environment(Some(false), true),
        )
        .expect("discovery"),
        nucleus_explore_interface::interface(name("explore")).expect("explore"),
        nucleus_fs::interface(name("fs")).expect("fs"),
        nucleus_lean_interface::interface(name("lean")).expect("lean"),
        nucleus_model_interface::interface(name("model")).expect("model"),
        nucleus_pdf_interface::interface(name("pdf")).expect("pdf"),
        nucleus_sessions_interface::interface(name("sessions")).expect("sessions"),
        nucleus_shell_interface::interface(name("shell"), String::new(), true).expect("shell"),
        nucleus_skills_interface::interface(name("skills")).expect("skills"),
        nucleus_swarm_interface::interface(name("swarm")).expect("swarm"),
        nucleus_web_interface::interface(name("web")).expect("web"),
    ]
}

/// Every name a template may legitimately put in backticks.
fn known() -> BTreeSet<String> {
    let mut known: BTreeSet<String> = VOCABULARY.iter().map(|s| (*s).to_owned()).collect();
    // The session's background jobs, offered by the agent itself like the
    // reminder tools.
    known.insert(nucleus_chat_agent::jobs::NAME.to_owned());
    // The duplicate check's own answer, offered only to the swarm service's
    // check.
    known.insert(nucleus_chat_agent::swarm::DUPLICATE_VERDICT_TOOL.to_owned());
    for decl in declarations() {
        for (action, spec) in &decl.actions {
            let Some(tool) = &spec.tool else { continue };
            known.insert(nucleus_chat_agent::swarm::advertised_name(&format!(
                "{}_{}",
                decl.endpoint.as_str(),
                action.as_str()
            )));
            if let Some(properties) = tool.params.get("properties").and_then(|p| p.as_object()) {
                known.extend(properties.keys().cloned());
            }
        }
    }
    known
}

/// Every tree of prompt text in the repository. The memory pipeline's prompts
/// are prompts: an extraction template that named a tool we do not have would
/// mislead the model writing our memories, which is worse than misleading one
/// doing a task — the mistake would be written down and read back later.
fn template_roots() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    vec![
        root.join("../prompts/templates"),
        root.join("../memories/read/templates"),
        root.join("../memories/write/templates"),
    ]
}

fn template_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            template_files(&path, out);
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("md" | "xml" | "toml")) {
            out.push(path);
        }
    }
}

/// The backticked spans of one template. Fenced blocks are skipped: a code
/// sample is an illustration, not a claim about this system's tools.
fn backticked(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else { break };
            found.push(after[..close].to_owned());
            rest = &after[close + 1..];
        }
    }
    found
}

/// True for a span that reads like an identifier of this system rather than
/// prose, a path or a shell fragment: the shape a tool name or a parameter has.
fn identifier_like(span: &str) -> bool {
    !span.is_empty()
        && span.len() <= 48
        && span.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && span.contains(['_', '-'])
        && !span.starts_with(['_', '-'])
        && !span.ends_with(['_', '-'])
}

#[test]
fn every_identifier_a_template_names_is_a_tool_that_exists() {
    let known = known();
    let mut files = Vec::new();
    for root in template_roots() {
        template_files(&root, &mut files);
    }
    assert!(!files.is_empty(), "no templates found under any of {:?}", template_roots());

    let mut unknown: Vec<String> = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read template");
        for span in backticked(&text) {
            if identifier_like(&span) && !known.contains(&span) {
                unknown.push(format!("{}: `{span}`", file.display()));
            }
        }
    }
    unknown.sort();
    unknown.dedup();
    assert!(
        unknown.is_empty(),
        "these templates name things this system does not have.\n\
         Either the name is wrong, or the tool is missing, or the term belongs in VOCABULARY:\n  {}",
        unknown.join("\n  ")
    );
}

/// The tools an agent is actually offered are derived from the capabilities it
/// holds, so the set differs per composition. This pins the derivation itself:
/// a provider that declares an action tool-callable is what puts it in front of
/// a model, and nothing in a prompt template can add one.
#[test]
fn a_tool_exists_because_its_provider_declared_it_not_because_a_prompt_says_so() {
    let tools: BTreeSet<String> = declarations()
        .iter()
        .flat_map(|decl| {
            decl.actions
                .iter()
                .filter(|(_, spec)| spec.tool.is_some())
                .map(|(action, _)| format!("{}_{}", decl.endpoint.as_str(), action.as_str()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(tools.contains("shell_exec"), "{tools:?}");
    assert!(tools.contains("explore_submit"), "{tools:?}");
    assert!(
        !tools.contains("model_complete"),
        "`model complete` is not offered as a tool: an agent's own model calls are not something \
         it calls a tool to make"
    );
}

/// Mechanisms this system has, as opposed to tools it offers.
///
/// A template may presuppose one of these. The list is short and every entry is
/// something a person can point at: a mechanism is here because it exists, not
/// because a prompt assumed it.
const MECHANISMS: &[&str] = &[
    // When the provider's own token count crosses the threshold, the chat loop
    // summarises its whole buffer (`program-files/chat-agent/src/lib.rs`) and
    // the prover resets it to the system message, the task and a notice, with
    // the work kept in files and no summary call
    // (`program-files/prover/src/agent.rs`). Both record the boundary so the
    // transcript replays to what the model saw.
    "compaction",
    // An operator-supplied JSON policy: ordered {endpoint, action, resource,
    // effect} rules, first match wins, no match is a refusal
    // (`program-files/approvals`, `--policy`).
    "approval-rules",
    // A swarm: the swarm service keeps a board of directions, decides each
    // registration with a bounded duplicate check, starts its members and wakes
    // the session that started it; a member's loop refuses other tools until it
    // holds a direction (`program-files/chat-agent`, the `swarm` module).
    "swarm",
];

/// A template may name a tool that exists, or a mechanism that exists. It may
/// not quietly assume either.
#[test]
fn every_template_declares_what_it_presupposes_and_nothing_claims_more_than_we_have() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../prompts/templates");
    let manifest: toml::Table =
        toml::from_str(&std::fs::read_to_string(root.join("manifest.toml")).expect("read manifest"))
            .expect("parse manifest");

    // Every template on disk has a row, and every row has a template.
    let mut files = Vec::new();
    template_files(&root, &mut files);
    let on_disk: BTreeSet<String> = files
        .iter()
        .filter_map(|p| p.strip_prefix(&root).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| p != "manifest.toml")
        .collect();
    let declared: BTreeSet<String> = manifest.keys().cloned().collect();
    let undeclared: Vec<&String> = on_disk.difference(&declared).collect();
    assert!(undeclared.is_empty(), "these templates have no row in manifest.toml: {undeclared:?}");
    let phantom: Vec<&String> = declared.difference(&on_disk).collect();
    assert!(phantom.is_empty(), "manifest.toml has rows for templates that do not exist: {phantom:?}");

    // A requirement is a tool we offer or a mechanism we have. Nothing else.
    let tools = known();
    let mut problems: Vec<String> = Vec::new();
    for (name, row) in &manifest {
        let row = row.as_table().unwrap_or_else(|| panic!("{name} is not a table"));
        let available = row["available"].as_bool().unwrap_or_else(|| panic!("{name}: available"));
        let requires: Vec<&str> = row["requires"]
            .as_array()
            .map(|a| a.iter().filter_map(toml::Value::as_str).collect())
            .unwrap_or_default();
        for requirement in requires {
            let is_tool = tools.contains(requirement);
            let is_mechanism = MECHANISMS.contains(&requirement);
            if available && !is_tool && !is_mechanism {
                problems.push(format!(
                    "{name} is marked available but needs `{requirement}`, which is neither a tool \
                     this system offers nor a mechanism it has"
                ));
            }
            if !available && (is_tool || is_mechanism) {
                problems.push(format!(
                    "{name} is marked unavailable but `{requirement}` exists; either it is \
                     available now, or it needs something else that is not written down"
                ));
            }
        }
    }
    problems.sort();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The chat agent's system prompt is assembled from these templates for what
/// it holds. Every combination it can render must fill every slot and name
/// only things that exist, and the permission words must follow the holdings.
#[test]
fn every_chat_agent_prompt_the_capabilities_can_select_is_complete_and_true() {
    use nucleus_chat_agent::{Held, instructions};
    use nucleus_prompts::Personality;
    let known = known();
    for bits in 0..32u8 {
        let held = Held {
            tools: 3,
            writes_files: bits & 1 != 0,
            guarded: bits & 2 != 0,
            network: bits & 4 != 0,
            swarms: bits & 8 != 0,
            jobs: bits & 16 != 0,
        };
        for personality in [Personality::Pragmatic, Personality::Friendly, Personality::None] {
            let text = instructions(personality, held);
            assert!(
                nucleus_prompts::unfilled(&text).is_empty(),
                "{held:?}: {:?}",
                nucleus_prompts::unfilled(&text)
            );
            for span in backticked(&text) {
                assert!(!identifier_like(&span) || known.contains(&span), "{held:?} names `{span}`");
            }
            let sandbox = if held.writes_files { "`workspace-write`" } else { "`read-only`" };
            let approval = if held.guarded { "`on-request`" } else { "`never`" };
            assert!(text.contains(sandbox) && text.contains(approval), "{held:?}");
        }
    }
}
