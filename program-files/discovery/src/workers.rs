//! What a run's process holds, and how it starts its workers: its authority,
//! found in its own handle table, each worker's program and grants, and the
//! workspace a continuation starts from (module doc, "Workers").

use super::*;

/// How the worker `task` for `problem`, of the run whose workers' directory is
/// `workers_dir`, is started (module doc, "Workers").
pub fn worker_program(
    options: &Options,
    workers_dir: &Path,
    problem: &str,
    task: &str,
) -> Result<ChildProgram, Error> {
    let mut args = options.prover_args.clone();
    args.extend(
        [
            "--out",
            WORKER_OUT,
            "--lean-endpoint",
            options.lean_endpoint.as_str(),
            "--explore-endpoint",
            options.explore_endpoint.as_str(),
            // The shell its run works in (the last one given is the one the
            // worker takes): the workers' shell with network, when the run's
            // task chose it.
            "--shell-endpoint",
            options.shell_endpoint.as_str(),
        ]
        .map(str::to_owned),
    );
    // The project its model calls are made for: its run's.
    if let Some(project) = &options.project {
        args.extend(["--project".to_owned(), project.clone()]);
    }
    Ok(ChildProgram {
        executable: path_text("the prover executable", &options.prover_executable)?,
        args,
        binds: vec![ChildBind {
            path: path_text("the worker's directory", &workers_dir.join(problem).join(task))?,
            mount: Some(format!("{WORKER_OUT}/{problem}/{task}")),
            writable: true,
        }],
        network: false,
    })
}

/// `path` as the text a kernel request carries.
pub(crate) fn path_text(what: &str, path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, format!("{what}: path is not UTF-8")))
}

/// The resource of `kernel spawn_executable` for the executable at `path`:
/// `["executable", <its segments>]`.
pub(crate) fn executable_resource(path: &Path) -> Result<ResourcePath, String> {
    let text = path.to_str().ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    ResourcePath::new(std::iter::once("executable").chain(text.split('/').filter(|s| !s.is_empty())))
        .map_err(|e| e.message)
}

/// A handle on `endpoint` holding every one of `actions`, delegable if asked,
/// whose scope contains `within` when given.
pub(crate) fn find<'a>(
    handles: &'a [HandleInfo],
    endpoint: &str,
    actions: &[&str],
    delegable: bool,
    within: Option<&ResourcePath>,
) -> Option<&'a HandleInfo> {
    handles.iter().find(|h| {
        h.endpoint.as_str() == endpoint
            && (!delegable || h.delegable)
            && actions.iter().all(|a| h.actions.iter().any(|held| held.as_str() == *a))
            && within.is_none_or(|target| target.is_within(&h.resource))
    })
}

/// The actions a run's process asks `lean` for itself, on `[key, problem]`
/// (the run's key, [`crate::wire::key_of`]), and narrows to its workers on
/// `[key, problem, task]`.
pub(crate) const LEAN_ACTIONS: [&str; 4] =
    [lean::ACTION_CHECK, lean::ACTION_VERIFY, lean::ACTION_ANALYZE, lean::ACTION_COMPLETE];
/// The `explore` actions a run's process narrows to its workers.
pub(crate) const EXPLORE_ACTIONS: [&str; 4] = [
    explore::ACTION_ASSIGNMENT,
    explore::ACTION_SUBMIT,
    explore::ACTION_PUBLISH_PLAN,
    explore::ACTION_READ_PLANS,
];

/// The capabilities a run's process uses and delegates, found in its own
/// handle table: its run's narrowing of the service's (module doc,
/// "Authority and compromise").
pub(crate) struct Authority {
    /// The run's key ([`wire::key_of`]): the first segment of its `lean`,
    /// `explore` and `report` resources, as the service scoped its report
    /// grant.
    pub(crate) key: String,
    /// `kernel provide` of its own endpoint.
    pub(crate) provide: HandleId,
    pub(crate) spawn: HandleId,
    pub(crate) model: HandleId,
    pub(crate) model_resource: ResourcePath,
    /// `model {next, cancel}`, when the generation handle also permits start.
    pub(crate) streaming: Option<HandleId>,
    /// `lean statement`, for a benchmark problem's own statement, `[problem]`.
    pub(crate) statement: HandleId,
    /// `lean {check, verify, analyze, complete}` on `[key]`, delegable.
    pub(crate) lean: HandleId,
    /// `lean register` on `[key]`, its own: it registers the run's
    /// statements and never delegates it.
    pub(crate) register: HandleId,
    pub(crate) explore: HandleId,
    /// `report` on the service's report endpoint, `[key]`.
    pub(crate) report: HandleId,
    /// A delegable `shell exec` on the run's workspaces, when the composition
    /// gives workers workspaces.
    pub(crate) shell: Option<HandleId>,
    /// The run's workspaces at the shell service: the resource of its shell
    /// grant, as the service chose it; a worker's is `[.., problem, task]`.
    pub(crate) workspaces: ResourcePath,
}

impl Authority {
    /// The authority of the process of `run`, which provides `endpoint` and
    /// reports to `report_endpoint`, on the run's key, the one segment its
    /// report grant names. Missing authority is refused with a message naming
    /// it.
    pub(crate) fn from(
        handles: &[HandleInfo],
        options: &Options,
        run: &str,
        endpoint: &EndpointName,
        report_endpoint: &EndpointName,
    ) -> Result<Self, String> {
        let missing = |what: String| format!("run {run} started without {what}");
        let report = find(handles, report_endpoint.as_str(), &[wire::ACTION_REPORT], false, None)
            .filter(|h| h.resource.segments().len() == 1)
            .ok_or_else(|| missing(format!("`{report_endpoint} report` on its key")))?;
        let key = report.resource.segments()[0].clone();
        let scope = report.resource.clone();
        let provide = ResourcePath::new(["endpoint", endpoint.as_str()]).map_err(|e| e.message)?;
        let provide = find(handles, KERNEL_ENDPOINT, &[ACTION_PROVIDE], false, Some(&provide))
            .ok_or_else(|| missing(format!("authority to provide its endpoint `{endpoint}`")))?;
        let executable = executable_resource(&options.prover_executable)?;
        let spawn = find(handles, KERNEL_ENDPOINT, &[ACTION_SPAWN_EXECUTABLE], false, Some(&executable))
            .ok_or_else(|| missing("authority to start the prover executable".to_owned()))?;
        let model = find(handles, model::ENDPOINT, &[model::ACTION_COMPLETE], true, None)
            .ok_or_else(|| missing("a delegable `model complete` capability".to_owned()))?;
        // Streaming is optional: a composition that does not delegate it
        // leaves its workers on the blocking `complete`, which works but
        // cannot tell a thinking provider from a stalled one.
        let streaming = if model.actions.iter().any(|a| a.as_str() == model::ACTION_START) {
            find(
                handles,
                model::ENDPOINT,
                &[model::ACTION_NEXT, model::ACTION_CANCEL],
                true,
                Some(&model.resource),
            )
            .map(|h| h.handle)
        } else {
            None
        };
        let lean_endpoint = options.lean_endpoint.as_str();
        let statement =
            find(handles, lean_endpoint, &[lean::ACTION_STATEMENT], false, Some(&ResourcePath::root()))
                .ok_or_else(|| missing(format!("`{lean_endpoint} statement` on the root")))?;
        let lean_h = find(handles, lean_endpoint, &LEAN_ACTIONS, true, Some(&scope)).ok_or_else(|| {
            missing(format!("a delegable `{lean_endpoint} {{check, verify, analyze, complete}}` on [{key}]"))
        })?;
        let register = find(handles, lean_endpoint, &[lean::ACTION_REGISTER], false, Some(&scope))
            .ok_or_else(|| missing(format!("`{lean_endpoint} register` on [{key}]")))?;
        // Every task with `w` of at least 2 coordinates on a plan board, so
        // the plan actions are required, not optional: coordination fails
        // closed rather than running such tasks uncoordinated.
        let explore_endpoint = options.explore_endpoint.as_str();
        let explore_h = find(handles, explore_endpoint, &EXPLORE_ACTIONS, true, Some(&scope)).ok_or_else(|| {
            missing(format!(
                "a delegable `{explore_endpoint} {{assignment, submit, publish_plan, read_plans}}` on [{key}]"
            ))
        })?;
        // Where the run's workspaces are is the service's choice: whatever
        // its delegable shell grant names, never the shell service's root.
        let shell_h = find(handles, options.shell_endpoint.as_str(), &[shell::ACTION_EXEC], true, None)
            .filter(|h| !h.resource.segments().is_empty());
        Ok(Self {
            key,
            provide: provide.handle,
            spawn: spawn.handle,
            model: model.handle,
            model_resource: model.resource.clone(),
            streaming,
            statement: statement.handle,
            lean: lean_h.handle,
            register: register.handle,
            explore: explore_h.handle,
            report: report.handle,
            shell: shell_h.map(|h| h.handle),
            workspaces: shell_h.map_or_else(ResourcePath::root, |h| h.resource.clone()),
        })
    }

    /// A worker's workspace at the shell service: the run's workspaces and
    /// `[problem, task]`, checked against the shell interface's plain
    /// alphabet so the grant names what the service will accept.
    pub(crate) fn workspace(&self, problem: &str, task: &str) -> Result<ResourcePath, String> {
        let mut segments = self.workspaces.segments().to_vec();
        segments.extend([problem.to_owned(), task.to_owned()]);
        let checked = shell::workspace_segments(&segments.join("/")).map_err(|e| e.message)?;
        ResourcePath::new(checked).map_err(|e| e.to_string())
    }

    /// One worker's whole authority: budgeted `model complete`, and `lean`,
    /// `explore` (with the plan actions when `plans`) on exactly its task
    /// `[key, problem, task]`, and, with a shell, `shell exec` on its own
    /// workspace. Without a shell the worker runs the fixed loop and gets
    /// budgeted `lean check`; with one, budgeted `lean verify` instead. Its
    /// `lean` resource names its run's registration, so what it is judged
    /// against is the run's statement and nothing it sends.
    pub(crate) fn grants_for(
        &self,
        problem: &str,
        task: &str,
        calls: u64,
        plans: bool,
    ) -> Result<Vec<Grant>, String> {
        let acts = |names: &[&str]| -> Result<BTreeSet<ActionName>, String> {
            names.iter().map(|n| ActionName::new(*n).map_err(|e| e.to_string())).collect()
        };
        let own = ResourcePath::new([self.key.as_str(), problem, task]).map_err(|e| e.to_string())?;
        let mut grants = vec![
            Grant {
                from: self.model,
                // `start` is a completion by another name, so it shares the
                // budget; without it the worker cannot stream at all.
                actions: if self.streaming.is_some() {
                    acts(&[model::ACTION_COMPLETE, model::ACTION_START])?
                } else {
                    acts(&[model::ACTION_COMPLETE])?
                },
                resource: self.model_resource.clone(),
                ttl_ms: None,
                delegable: false,
                max_calls: Some(calls),
            },
            Grant {
                from: self.explore,
                actions: if plans {
                    acts(&EXPLORE_ACTIONS)?
                } else {
                    acts(&[explore::ACTION_ASSIGNMENT, explore::ACTION_SUBMIT])?
                },
                resource: own.clone(),
                ttl_ms: None,
                delegable: false,
                max_calls: None,
            },
        ];
        // Pulling a stream is not a completion: `next` and `cancel` ride on
        // their own unbudgeted capability, or every poll would be charged
        // against the worker's model calls and the budget would vanish into
        // polling (the model interface says so in as many words).
        if let Some(streaming) = self.streaming {
            grants.push(Grant {
                from: streaming,
                actions: acts(&[model::ACTION_NEXT, model::ACTION_CANCEL])?,
                resource: self.model_resource.clone(),
                ttl_ms: None,
                delegable: false,
                max_calls: None,
            });
        }
        // Without a shell there is no tool loop: the worker runs the fixed
        // candidate/verdict loop, whose evaluator *is* `lean check`. The tool
        // loop's worker holds no `check`: it writes files
        // and runs `lean` on them, and the protocol is enforced where it
        // matters, at the submission.
        match self.shell {
            None => grants.push(Grant {
                from: self.lean,
                actions: acts(&[lean::ACTION_CHECK])?,
                resource: own,
                ttl_ms: None,
                delegable: false,
                max_calls: Some(calls.saturating_mul(4).saturating_add(4)),
            }),
            Some(shell) => {
                grants.push(Grant {
                    from: shell,
                    actions: acts(&[shell::ACTION_EXEC])?,
                    resource: self.workspace(problem, task)?,
                    ttl_ms: None,
                    delegable: false,
                    max_calls: None,
                });
                grants.push(Grant {
                    from: self.lean,
                    actions: acts(&[lean::ACTION_VERIFY])?,
                    resource: own,
                    ttl_ms: None,
                    delegable: false,
                    max_calls: Some(calls.saturating_mul(2).saturating_add(2)),
                });
            }
        }
        Ok(grants)
    }
}

/// The command that starts a child's workspace from its parent's, run in the
/// child's workspace (`prepare_workspace`): `pwd` first, and only to learn
/// where the workspace is, because the provider cuts the tail of a capped
/// output and keeps the head; then the copy. The copy keeps the parent's
/// modes, and a parent that made its workspace or a file read-only would
/// leave every child unable to write its own, so the owner's rights are
/// restored whether or not the copy succeeded; the command exits with the
/// copy's status. The parent's task id is one of the service's own
/// (`t000007`), a plain word that needs no quoting.
pub(crate) fn clone_command(parent_task: &str) -> String {
    format!("pwd && {{ cp -a ../{parent_task}/. .; copied=$?; chmod -R u+rwX . && exit $copied; }}")
}

/// The workspace's absolute path in the output of a command that begins with
/// `pwd`: its first non-empty line. A run's process never learns the shell
/// service's root any other way — the root is that service's configuration,
/// and a capability names a workspace by resource path, not by directory.
///
/// `None` when that line is not an absolute path, or holds a character JSON
/// escapes (a quote, a backslash, a control character), because
/// [`retarget_transcript`] substitutes it into JSONL text as it stands. Both
/// are unreachable for a sane root; the caller treats them as a workspace it
/// cannot continue from rather than guessing.
pub(crate) fn workspace_dir_in(output: &str) -> Option<&str> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    let plain = |c: char| !c.is_control() && c != '"' && c != '\\';
    (line.starts_with('/') && line.chars().all(plain)).then_some(line)
}

/// A parent's transcript as its child should read it: every mention of the
/// parent's workspace directory rewritten to the child's, `child_dir`.
///
/// The two are siblings under the shell service's root — the parent's is
/// `child_dir`'s directory plus the parent's task id, the same relation the
/// copy in [`clone_command`] relies on. The rewrite is a plain string
/// replacement over the JSONL text: both paths are absolute, their last
/// segment is a minted task id (`t000001`, fixed width, so no workspace path
/// is a prefix of a sibling's), and neither can appear in Lean source. It
/// cannot break the JSON, because what it removes and what it inserts are both
/// plain characters inside a string ([`workspace_dir_in`] rejects anything
/// else).
pub(crate) fn retarget_transcript(text: &str, child_dir: &str, parent_task: &str) -> String {
    let Some((base, _)) = child_dir.rsplit_once('/') else { return text.to_owned() };
    text.replace(&format!("{base}/{parent_task}"), child_dir)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::tests::{TEST_PROVER, base_args, parse};

    #[test]
    fn a_worker_runs_the_prover_executable_in_the_workers_directory_with_no_network() {
        let mut args = base_args();
        args.extend(["--prover-arg", "--model", "--prover-arg", "m", "--lean-endpoint", "lean2"]);
        args.extend(["--project", "p"]);
        let options = parse(&args).unwrap();
        let program = worker_program(&options, Path::new("/out/gen2.workers"), "p1", "t000003").unwrap();
        assert_eq!(program.executable, TEST_PROVER);
        // Only its own directory, at the path it writes: never a sibling's.
        assert_eq!(
            program.binds,
            [ChildBind {
                path: "/out/gen2.workers/p1/t000003".into(),
                mount: Some("/out/p1/t000003".into()),
                writable: true
            }]
        );
        assert!(!program.network);
        // The prover arguments, the run's endpoints and its project, which its
        // model calls name.
        let given = |flag: &str, value: &str| program.args.windows(2).any(|pair| pair == [flag, value]);
        assert!(
            given("--model", "m") && given("--lean-endpoint", "lean2") && given("--project", "p"),
            "{:?}",
            program.args
        );
        let json = serde_json::to_value(&program).unwrap();
        assert_eq!(json["binds"][0]["mount"], "/out/p1/t000003");
        assert_eq!(json["network"], false);
    }

    /// A continuation reads its parent's transcript as its own history, so
    /// the parent's workspace path in it has to become the child's, or the
    /// child works in its parent's directory and returns its own untouched.
    #[test]
    fn a_childs_copy_of_its_parents_transcript_names_the_childs_own_workspace() {
        // What `pwd && cp -a …` answers when the copy worked: the workspace,
        // then nothing (`cp` is silent on success).
        let child = workspace_dir_in("/srv/ws/run1/putnam_2023_b5/t000002\n").unwrap();
        assert_eq!(child, "/srv/ws/run1/putnam_2023_b5/t000002");
        let message = |content: &str| format!(r#"{{"message":{{"role":"user","content":"{content}"}}}}"#);
        let parent = [
            message("cd /srv/ws/run1/putnam_2023_b5/t000001 && lean b5.lean"),
            message("wrote /srv/ws/run1/putnam_2023_b5/t000001/head.lean"),
            message("theorem putnam_2023_b5 : t000001 = 1 := by sorry"),
        ]
        .join("\n");
        let copied = retarget_transcript(&parent, child, "t000001");
        assert_eq!(
            copied.matches("/srv/ws/run1/putnam_2023_b5/t000002").count(),
            2,
            "both of the parent's paths now name the child's workspace: {copied}"
        );
        assert!(
            !copied.contains("/srv/ws/run1/putnam_2023_b5/t000001"),
            "nothing still points at the parent: {copied}"
        );
        assert!(copied.contains("t000001 = 1"), "a bare task id in Lean source is untouched: {copied}");
        for line in copied.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        // No workspace path the harness can trust means no inherited
        // transcript at all: `prepare_workspace` records it and the child
        // starts fresh rather than inheriting the parent's directory.
        assert_eq!(workspace_dir_in("cp: cannot stat '../t000001/.': No such file"), None);
        assert_eq!(workspace_dir_in(""), None);
        assert_eq!(workspace_dir_in("/srv/ws/a\"b/run1/p/t000002"), None, "a path JSON would escape");

        // The copy keeps the parent's modes, and a parent that made its
        // workspace or its files read-only (or unreadable) must not leave its
        // child unable to write its own: the owner's rights come back whether
        // or not the copy itself succeeded.
        let root = crate::tests::scratch("clone");
        let (parent, child) = (root.join("t000001"), root.join("t000002"));
        std::fs::create_dir_all(parent.join("lemmas")).unwrap();
        std::fs::create_dir(&child).unwrap();
        std::fs::write(parent.join("b5.lean"), "theorem").unwrap();
        std::fs::write(parent.join("lemmas/hidden.lean"), "lemma").unwrap();
        let mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap()
        };
        mode(&parent.join("b5.lean"), 0o444);
        mode(&parent.join("lemmas/hidden.lean"), 0o000);
        mode(&parent.join("lemmas"), 0o555);
        mode(&parent, 0o555);
        let ran = std::process::Command::new("/bin/sh")
            .args(["-c", &clone_command("t000001")])
            .current_dir(&child)
            .output()
            .unwrap();
        let printed = String::from_utf8_lossy(&ran.stdout);
        assert_eq!(workspace_dir_in(&printed).map(Path::new), Some(child.canonicalize().unwrap().as_path()));
        for copied in [child.clone(), child.join("b5.lean"), child.join("lemmas")] {
            let bits =
                std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&copied).unwrap().permissions());
            assert_eq!(bits & 0o600, 0o600, "{} is its owner's to write: {bits:o}", copied.display());
        }
        mode(&parent, 0o755);
        mode(&parent.join("lemmas"), 0o755);
        mode(&parent.join("lemmas/hidden.lean"), 0o644);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The key of run `run`, as the service scoped the report grant.
    const KEY: &str = "run-00112233445566778899";

    #[test]
    fn a_run_process_holds_its_runs_narrowing_and_its_workers_are_judged_on_their_run() {
        let handle = |id, endpoint, actions: &[&str], resource: &[&str], delegable| HandleInfo {
            handle: HandleId(id),
            endpoint: EndpointName::new(endpoint).unwrap(),
            actions: actions.iter().map(|a| ActionName::new(*a).unwrap()).collect(),
            resource: ResourcePath::new(resource.iter().copied()).unwrap(),
            delegable,
            expires_in_ms: None,
            max_calls: None,
        };
        let options = parse(&base_args()).unwrap();
        let (endpoint, report) =
            (EndpointName::new("dynamic.7").unwrap(), EndpointName::new("dynamic.2").unwrap());
        let stream = [model::ACTION_COMPLETE, model::ACTION_START, model::ACTION_NEXT, model::ACTION_CANCEL];
        let mut handles = vec![
            handle(1, "kernel", &[ACTION_PROVIDE], &["endpoint", "dynamic.7"], false),
            handle(
                2,
                "kernel",
                &[ACTION_SPAWN_EXECUTABLE],
                &["executable", "programs", "nucleus-prover"],
                false,
            ),
            handle(3, "model", &stream, &["deepseek"], true),
            handle(4, "lean", &[lean::ACTION_STATEMENT], &[], false),
            handle(5, "lean", &LEAN_ACTIONS, &[KEY], true),
            handle(6, "explore", &[explore::ACTION_ASSIGNMENT, explore::ACTION_SUBMIT], &[KEY], true),
            handle(8, "dynamic.2", &[wire::ACTION_REPORT], &[KEY], false),
        ];
        let refused = |handles: &[HandleInfo], endpoint: &EndpointName| {
            Authority::from(handles, &options, "run", endpoint, &report).err().unwrap_or_default()
        };
        assert!(refused(&handles, &endpoint).contains("register"), "{}", refused(&handles, &endpoint));
        // `register` is the run's own: it is not delegable.
        handles.push(handle(7, "lean", &[lean::ACTION_REGISTER], &[KEY], false));
        assert!(
            refused(&handles, &endpoint).contains("publish_plan"),
            "coordination fails closed without the plan actions"
        );
        for action in [explore::ACTION_PUBLISH_PLAN, explore::ACTION_READ_PLANS] {
            handles[5].actions.insert(ActionName::new(action).unwrap());
        }
        // The key is the report grant's own segment: another endpoint's narrowing is not this one's.
        let other = EndpointName::new("dynamic.8").unwrap();
        assert!(
            refused(&handles, &other).contains("run run started without"),
            "{}",
            refused(&handles, &other)
        );
        // The workers' workspaces are where the service's shell grant says.
        let workspaces = ["projects", "p", "runs", "run"];
        handles.push(handle(9, options.shell_endpoint.as_str(), &[shell::ACTION_EXEC], &workspaces, true));
        let authority = Authority::from(&handles, &options, "run", &endpoint, &report).unwrap();
        assert_eq!(
            (authority.key.as_str(), authority.statement, authority.register, authority.report),
            (KEY, HandleId(4), HandleId(7), HandleId(8))
        );
        for plans in [false, true] {
            let grants = authority.grants_for("demo", "t000001", 10, plans).unwrap();
            let explore_grant = grants.iter().find(|g| g.from == HandleId(6)).unwrap();
            assert_eq!(explore_grant.resource, ResourcePath::new([KEY, "demo", "t000001"]).unwrap());
            assert!(!explore_grant.delegable);
            for action in [explore::ACTION_PUBLISH_PLAN, explore::ACTION_READ_PLANS] {
                assert_eq!(explore_grant.actions.contains(&ActionName::new(action).unwrap()), plans);
            }
            // The worker's `lean` names its run's registration, never a bare
            // problem id; it never holds `register`, `statement` or `report`.
            let lean_grant = grants.iter().find(|g| g.from == HandleId(5)).unwrap();
            assert_eq!(lean_grant.resource, ResourcePath::new([KEY, "demo", "t000001"]).unwrap());
            assert!(grants.iter().all(|g| ![HandleId(4), HandleId(7), HandleId(8)].contains(&g.from)));
            let shell_grant = grants.iter().find(|g| g.from == HandleId(9)).unwrap();
            let own = ["projects", "p", "runs", "run", "demo", "t000001"];
            assert_eq!(
                (&shell_grant.resource, shell_grant.delegable),
                (&ResourcePath::new(own).unwrap(), false)
            );
            // One model handle, two grants: the generation budgeted, the
            // stream pulled without charging it.
            let model_grants: Vec<Option<u64>> =
                grants.iter().filter(|g| g.from == HandleId(3)).map(|g| g.max_calls).collect();
            assert_eq!(model_grants, [Some(10), None]);
        }
        handles[4].delegable = false;
        assert!(refused(&handles, &endpoint).contains("delegable"));
    }
}
