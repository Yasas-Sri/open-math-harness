//! Executed architecture check: dependency direction between workspace crates.
//!
//! The kernel must not depend on agents, services, SDKs, the platform, or the
//! CLI, and services and agents must reach the kernel only through the public
//! protocol and SDK. A new crate fails this test until a rule is written for it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// Allowed normal (non-dev, non-build) dependencies per workspace crate.
/// `None` for external crates means "any"; the kernel is the exception and
/// lists every dependency it may have.
fn rules() -> BTreeMap<&'static str, (&'static [&'static str], Option<&'static [&'static str]>)> {
    BTreeMap::from([
        ("nucleus-protocol", (&[][..], None)),
        ("nucleus-kernel", (&["nucleus-protocol"][..], Some(&[][..]))),
        ("nucleus-sdk", (&["nucleus-protocol"][..], None)),
        ("nucleus-platform", (&["nucleus-kernel", "nucleus-protocol", "nucleus-preexec"][..], None)),
        ("nucleus-fs", (&["nucleus-protocol", "nucleus-sdk"][..], None)),
        ("nucleus-plugin-development", (&["nucleus-protocol", "nucleus-sdk"][..], None)),
        ("nucleus-mcp", (&["nucleus-protocol", "nucleus-sdk"][..], None)),
        ("nucleus-model-interface", (&["nucleus-protocol"][..], None)),
        // The reasoning effort a session runs at is the model interface's.
        ("nucleus-conversation-interface", (&["nucleus-model-interface", "nucleus-protocol"][..], None)),
        (
            "nucleus-conversation",
            (
                &[
                    "nucleus-conversation-interface",
                    // The names of projects a session belongs to.
                    "nucleus-projects-interface",
                    "nucleus-protocol",
                    "nucleus-sdk",
                ][..],
                None,
            ),
        ),
        (
            "nucleus-chat-agent",
            (
                &[
                    "nucleus-approvals-interface",
                    "nucleus-conversation-interface",
                    // The read half of memory, and only the read half: an
                    // agent is told what is remembered and says what it used.
                    // The write pipeline is a service's, and this crate
                    // cannot reach it.
                    "nucleus-memories-read",
                    "nucleus-memory-interface",
                    "nucleus-model-interface",
                    // An attached PDF's text, through the session's own `pdf` tool.
                    "nucleus-pdf-interface",
                    // The prompt assembly. A leaf that holds no authority: the
                    // agent takes text from it, not capability.
                    "nucleus-prompts",
                    // Where a session works in its project.
                    "nucleus-projects-interface",
                    "nucleus-protocol",
                    "nucleus-sdk",
                    "nucleus-sessions-interface",
                    // The swarm service prepares its members' directories
                    // with a shell command.
                    "nucleus-shell-interface",
                    // The skills its instructions list, and a person's /skill
                    // command, which it answers with its own load.
                    "nucleus-skills-interface",
                    "nucleus-swarm-interface",
                ][..],
                None,
            ),
        ),
        // The only crate allowed to contain `unsafe`: it holds the Linux `pre_exec` hooks
        // (a program's lifetime and cgroup, inherited bind descriptors, a
        // command's child subreaper) and depends on nothing of ours.
        ("nucleus-preexec", (&[][..], None)),
        // The setuid-root helper: root code, so nothing of ours and nothing
        // beyond the system-call wrappers it needs.
        ("nucleus-as", (&[][..], Some(&["rustix"][..]))),
        ("nucleus-approvals-interface", (&["nucleus-protocol"][..], None)),
        (
            // The guard holds real authority over every protected action, so
            // what it may link is worth reading twice. It makes no model call.
            "nucleus-approvals",
            (&["nucleus-approvals-interface", "nucleus-protocol", "nucleus-sdk"][..], None),
        ),
        // A session's agent has a reasoning effort, the model interface's.
        ("nucleus-sessions-interface", (&["nucleus-model-interface", "nucleus-protocol"][..], None)),
        (
            "nucleus-ui",
            (
                &[
                    "nucleus-approvals-interface",
                    "nucleus-conversation-interface",
                    // Read-only: Review shows the git diff of the person's
                    // repositories; the interface has no action that writes.
                    "nucleus-diffs-interface",
                    "nucleus-discovery-interface",
                    // Read-only: the window shows what is remembered. There
                    // is no operation that writes a memory.
                    "nucleus-memory-interface",
                    // The person's own projects and files.
                    "nucleus-projects-interface",
                    "nucleus-protocol",
                    // The scan of the sessions' records: one action, for the
                    // platform's connection only.
                    "nucleus-publish-interface",
                    "nucleus-sdk",
                    "nucleus-sessions-interface",
                    // A session's background jobs: listed, read, stopped, and a
                    // person's move of a running command.
                    "nucleus-shell-interface",
                    // Read-only: the skills a /skill command may name.
                    "nucleus-skills-interface",
                    // Asks for the next message a window may offer; the data only.
                    "nucleus-suggestions-interface",
                    // Read-only: a swarm's status and records for the team's
                    // card; no window starts, judges or stops one.
                    "nucleus-swarm-interface",
                ][..],
                None,
            ),
        ),
        ("nucleus-suggestions-interface", (&["nucleus-protocol"][..], None)),
        // The suggestions service: its own model call on the messages it is
        // sent. No session, conversation or memory crate: it cannot reach one.
        (
            "nucleus-suggestions",
            (
                &[
                    "nucleus-model-interface",
                    "nucleus-prompts",
                    "nucleus-protocol",
                    "nucleus-sdk",
                    "nucleus-suggestions-interface",
                ][..],
                None,
            ),
        ),
        ("nucleus-publish-interface", (&["nucleus-protocol"][..], None)),
        // The publish service: it reads sessions through their
        // interface and scans them. No model, session, shell or fs crate: it
        // calls none and opens no project's home.
        (
            "nucleus-publish",
            (
                &[
                    "nucleus-conversation-interface",
                    "nucleus-projects-interface",
                    "nucleus-protocol",
                    "nucleus-publish-interface",
                    "nucleus-sdk",
                ][..],
                None,
            ),
        ),
        ("nucleus-openrouter", (&["nucleus-model-interface", "nucleus-protocol", "nucleus-sdk"][..], None)),
        ("nucleus-web-interface", (&["nucleus-protocol"][..], None)),
        ("nucleus-shell-interface", (&["nucleus-protocol"][..], None)),
        // The shell service: commands in per-workspace directories through the public protocol and SDK only.
        (
            "nucleus-shell",
            (&["nucleus-preexec", "nucleus-protocol", "nucleus-sdk", "nucleus-shell-interface"][..], None),
        ),
        ("nucleus-web", (&["nucleus-protocol", "nucleus-sdk", "nucleus-web-interface"][..], None)),
        ("nucleus-pdf-interface", (&["nucleus-protocol"][..], None)),
        ("nucleus-pdf", (&["nucleus-pdf-interface", "nucleus-protocol", "nucleus-sdk"][..], None)),
        ("nucleus-skills-interface", (&["nucleus-protocol"][..], None)),
        // A project's skills, as the project's user; its fetches are the web
        // service's own code, linked, with the same checks.
        (
            "nucleus-skills",
            (&["nucleus-protocol", "nucleus-sdk", "nucleus-skills-interface", "nucleus-web"][..], None),
        ),
        ("nucleus-diffs-interface", (&["nucleus-protocol"][..], None)),
        ("nucleus-projects-interface", (&["nucleus-protocol"][..], None)),
        // A swarm's records name its members' reasoning effort, the model
        // interface's.
        ("nucleus-swarm-interface", (&["nucleus-model-interface", "nucleus-protocol"][..], None)),
        // A project's host for git and files, as the project's user: it runs
        // git.
        ("nucleus-project", (&["nucleus-projects-interface", "nucleus-protocol", "nucleus-sdk"][..], None)),
        // The projects router: it declares at its own endpoints the
        // interfaces of the programs it starts as hosts (fs, pdf, shell,
        // skills, diffs, the project host) and runs none of them itself.
        (
            "nucleus-projects",
            (
                &[
                    // A background job's end wakes its session.
                    "nucleus-conversation-interface",
                    "nucleus-diffs-interface",
                    "nucleus-fs",
                    "nucleus-pdf-interface",
                    "nucleus-projects-interface",
                    "nucleus-protocol",
                    "nucleus-sdk",
                    "nucleus-shell",
                    "nucleus-shell-interface",
                    "nucleus-skills-interface",
                ][..],
                None,
            ),
        ),
        // Reads the workspace's repositories by running git; no other
        // service of ours.
        ("nucleus-diffs", (&["nucleus-diffs-interface", "nucleus-protocol", "nucleus-sdk"][..], None)),
        ("nucleus-lean-interface", (&["nucleus-protocol"][..], None)),
        // The explore interface carries the lean check verdict inside assignments.
        ("nucleus-explore-interface", (&["nucleus-lean-interface", "nucleus-protocol"][..], None)),
        // The discovery interface: the task document, its validation and the
        // run records. Data only, on the protocol alone.
        ("nucleus-discovery-interface", (&["nucleus-protocol"][..], None)),
        // The headless operator client of the discovery service: the same
        // interface and authorization path as a chat agent, through the SDK.
        (
            "nucleus-discovery-client",
            (&["nucleus-discovery-interface", "nucleus-protocol", "nucleus-sdk"][..], None),
        ),
        // The discovery harness: Dream-RSI's tree search. It drives its
        // exploration policy, serves the discovery and explore interfaces, and
        // reaches the kernel through the public protocol and SDK only.
        (
            "nucleus-discovery",
            (
                &[
                    "nucleus-discovery-interface",
                    "nucleus-explore-interface",
                    "nucleus-lean-interface",
                    "nucleus-model-interface",
                    "nucleus-protocol",
                    "nucleus-sdk",
                    "nucleus-shell-interface",
                ][..],
                None,
            ),
        ),
        // The prover agent: model, lean and shell (+ explore, as a continuation
        // worker) through the public protocol and SDK only. Its external
        // dependencies are unrestricted, which admits `minijinja`, the
        // system-prompt template engine.
        (
            "nucleus-prover",
            (
                &[
                    "nucleus-explore-interface",
                    "nucleus-lean-interface",
                    "nucleus-model-interface",
                    "nucleus-protocol",
                    "nucleus-sdk",
                    "nucleus-shell-interface",
                ][..],
                None,
            ),
        ),
        // The Lean verifier service: the pure protocol core plus the provider
        // loop. It reaches the kernel only through the public protocol and SDK.
        ("nucleus-lean", (&["nucleus-lean-interface", "nucleus-protocol", "nucleus-sdk"][..], None)),
        (
            "nucleus-fixture-agent",
            (&["nucleus-model-interface", "nucleus-protocol", "nucleus-sdk"][..], None),
        ),
        // The prompt assembly is a leaf: text, and the logic that fills its
        // slots. It depends on nothing in this workspace and nothing outside
        // it. That is the point -- it must be usable by any agent without
        // dragging a kernel, an interface or a model SDK behind it, and it must
        // not be able to reach for one. What the templates are allowed to *say*
        // is checked separately, against the interfaces, by `prompts.rs`.
        ("nucleus-prompts", (&[][..], Some(&[][..]))),
        // The memory read path is a leaf for the same reason and one more: a
        // session that reads what is remembered must not be able to write it.
        // It cannot depend on the write pipeline because it cannot depend on
        // anything.
        ("nucleus-memories-read", (&[][..], Some(&[][..]))),
        // The write pipeline holds no nucleus crate either: it is prompts, a
        // job table and a directory of files. It does not depend on the read
        // path, because the two are joined by the store and not by a call, and
        // it must not depend on a kernel or a model client -- what calls a
        // model with these prompts is a service, and the service is what holds
        // the capability to do it.
        ("nucleus-memories-write", (&[][..], Some(&["rusqlite", "serde", "serde_json", "similar"][..]))),
        ("nucleus-memory-interface", (&["nucleus-protocol"][..], None)),
        // The memory service: what holds the capabilities the two halves of
        // the pipeline cannot hold themselves -- a model budget, the sessions
        // it may read, the workspace, and authority to start the one agent
        // that edits it. Through the public protocol and SDK only.
        (
            "nucleus-memory",
            (
                &[
                    "nucleus-conversation-interface",
                    "nucleus-memories-read",
                    "nucleus-memories-write",
                    "nucleus-memory-interface",
                    "nucleus-model-interface",
                    "nucleus-projects-interface",
                    "nucleus-protocol",
                    "nucleus-sdk",
                ][..],
                None,
            ),
        ),
        ("nucleus-cli", (&["nucleus-kernel", "nucleus-platform", "nucleus-protocol"][..], None)),
        // The UI test client consumes the public history pagination contract;
        // this is a test-harness dependency, never a kernel/launcher dependency.
        (
            "nucleus-e2e",
            (
                &["nucleus-conversation-interface", "nucleus-kernel", "nucleus-platform", "nucleus-protocol"]
                    [..],
                None,
            ),
        ),
    ])
}

#[test]
fn workspace_dependencies_point_in_the_allowed_direction() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_program_process_boundaries(&metadata);

    let rules = rules();
    let mut seen = BTreeSet::new();
    for package in metadata["packages"].as_array().unwrap() {
        let name = package["name"].as_str().unwrap();
        seen.insert(name.to_owned());
        let (internal_allowed, external_allowed) =
            rules.get(name).unwrap_or_else(|| panic!("{name} has no dependency rule; add one deliberately"));

        for dependency in package["dependencies"].as_array().unwrap() {
            if !dependency["kind"].is_null() {
                continue; // dev and build dependencies do not ship in the crate
            }
            let dep = dependency["name"].as_str().unwrap();
            if dep.starts_with("nucleus-") {
                assert!(internal_allowed.contains(&dep), "{name} must not depend on {dep}");
            } else if let Some(allowed) = external_allowed {
                assert!(allowed.contains(&dep), "{name} must not depend on external crate {dep}");
            }
        }
    }
    for name in rules.keys() {
        // A rule for a crate that no longer exists is stale documentation.
        assert!(seen.contains(*name), "rule for {name} matches no workspace crate");
    }
}

/// Traverse normal workspace dependencies only. Dev dependencies can link
/// program libraries into test binaries, but never into the shipped launcher.
fn normal_dependency_graph(metadata: &serde_json::Value) -> BTreeMap<String, BTreeSet<String>> {
    let packages = metadata["packages"].as_array().unwrap();
    let names: BTreeSet<_> = packages.iter().map(|package| package["name"].as_str().unwrap()).collect();
    packages
        .iter()
        .map(|package| {
            let dependencies = package["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|dependency| dependency["kind"].is_null())
                .map(|dependency| dependency["name"].as_str().unwrap())
                .filter(|name| names.contains(name))
                .map(str::to_owned)
                .collect();
            (package["name"].as_str().unwrap().to_owned(), dependencies)
        })
        .collect()
}

fn dependency_closure<'a>(graph: &'a BTreeMap<String, BTreeSet<String>>, root: &'a str) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(name) = pending.pop() {
        if !seen.insert(name) {
            continue;
        }
        if let Some(dependencies) = graph.get(name) {
            pending.extend(dependencies.iter().map(String::as_str));
        }
    }
    seen.remove(root);
    seen
}

/// Every crate under program-files/ is an independently runnable program: it
/// has its own binary target in its directory and never links the kernel or
/// the platform, and neither nucleus-cli nor nucleus-kernel links any of them,
/// counting normal dependencies transitively.
fn assert_program_process_boundaries(metadata: &serde_json::Value) {
    let program_files = Path::new(metadata["workspace_root"].as_str().unwrap()).join("program-files");
    let graph = normal_dependency_graph(metadata);
    let mut programs = BTreeSet::new();
    for package in metadata["packages"].as_array().unwrap() {
        let manifest = Path::new(package["manifest_path"].as_str().unwrap());
        if !manifest.starts_with(&program_files) {
            continue;
        }
        let name = package["name"].as_str().unwrap();
        programs.insert(name);
        assert!(
            package["targets"].as_array().unwrap().iter().any(|target| {
                target["kind"].as_array().unwrap().iter().any(|kind| kind == "bin")
                    && Path::new(target["src_path"].as_str().unwrap()).starts_with(manifest.parent().unwrap())
            }),
            "{name} must have its own executable in program-files, launched as a separate process"
        );
        let linked = dependency_closure(&graph, name);
        for forbidden in ["nucleus-kernel", "nucleus-platform"] {
            assert!(!linked.contains(forbidden), "{name} must not link {forbidden}, even indirectly");
        }
    }
    assert!(!programs.is_empty(), "program-files must contain the independent programs");
    for trusted in ["nucleus-cli", "nucleus-kernel"] {
        assert!(graph.contains_key(trusted));
        let linked = dependency_closure(&graph, trusted);
        let embedded: Vec<_> = linked.intersection(&programs).collect();
        assert!(embedded.is_empty(), "{trusted} must not embed program implementations: {embedded:?}");
    }
}

#[test]
fn closure_check_catches_indirect_program_links_and_excludes_dev_dependencies() {
    let metadata = serde_json::json!({"packages":[
        {"name":"launcher","dependencies":[{"name":"helper","kind":null},{"name":"test-only","kind":"dev"}]},
        {"name":"helper","dependencies":[{"name":"program","kind":null}]},
        {"name":"program","dependencies":[{"name":"kernel","kind":null}]},
        {"name":"kernel","dependencies":[]},
        {"name":"test-only","dependencies":[]}
    ]});
    let graph = normal_dependency_graph(&metadata);
    assert_eq!(dependency_closure(&graph, "launcher"), BTreeSet::from(["helper", "program", "kernel"]));
    assert_eq!(dependency_closure(&graph, "helper"), BTreeSet::from(["program", "kernel"]));
}
