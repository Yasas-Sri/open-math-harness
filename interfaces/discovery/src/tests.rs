#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use serde_json::Value;

fn ceilings() -> Ceilings {
    Ceilings {
        max_concurrency: 4,
        max_usd_micro_per_problem: 20_000_000,
        max_problems_per_task: 100,
        max_width: 16,
    }
}

fn task() -> Task {
    Task {
        statements: None,
        v: 1,
        name: "gen2-heldout".into(),
        problems: Some(vec!["putnam_1962_b2".into(), "putnam_1985_a1".into()]),
        problems_file: None,
        range: None,
        prompt_template: Some("{{ statement }}".into()),
        prompt_template_file: None,
        policy: Some("mine/pi_custom.py".into()),
        concurrency: 2,
        limits: TaskLimits::default(),
        worker_network: None,
        timeout_ms: None,
    }
}

fn refused(task: Task) -> TaskError {
    task.validate(&ceilings()).unwrap_err()
}

#[test]
fn the_interface_declares_seven_tool_actions() {
    let decl =
        interface(EndpointName::new(ENDPOINT).unwrap(), &ceilings(), environment(None, false)).unwrap();
    assert_eq!(decl.version, VERSION);
    let by_run = ResourceFrom::Segments { param: "run".to_owned() };
    let expected = [
        (ACTION_START, EffectClass::Irreversible, ResourceFrom::Handle),
        (ACTION_RESUME, EffectClass::Irreversible, by_run.clone()),
        (ACTION_STATUS, EffectClass::Observation, by_run.clone()),
        (ACTION_STOP, EffectClass::Irreversible, by_run.clone()),
        (ACTION_LIST, EffectClass::Observation, ResourceFrom::Handle),
        (ACTION_PROBLEMS, EffectClass::Observation, by_run),
        (ACTION_TREE, EffectClass::Observation, ResourceFrom::Segments { param: "path".to_owned() }),
    ];
    assert_eq!(decl.actions.len(), expected.len() + 1, "and `forget`, the UI service's");
    assert!(decl.actions[&ActionName::new(ACTION_FORGET).unwrap()].tool.is_none());
    for (name, effect, from) in expected {
        let action = &decl.actions[&ActionName::new(name).unwrap()];
        assert_eq!(action.effect, effect, "{name}");
        let tool = action.tool.as_ref().unwrap_or_else(|| panic!("{name} is a tool"));
        assert_eq!(tool.resource_from, from, "{name}");
        assert!(!tool.description.is_empty() && tool.params["type"] == "object", "{name}");
    }
    let start = decl.actions[&ActionName::new(ACTION_START).unwrap()].tool.clone().unwrap();
    assert_eq!(start.params["required"], json!(["v", "name", "concurrency"]));
    for field in ["problems", "problems_file", "range", "prompt_template", "policy", "limits"] {
        assert!(start.params["properties"].get(field).is_some(), "{field} in the schema");
    }
    // The ceilings a task is validated against are in the schema, numbers
    // included, so a model need not learn them from a refusal.
    let concurrency = &start.params["properties"]["concurrency"];
    let width = &start.params["properties"]["limits"]["properties"]["w"];
    for (field, ceiling) in [(concurrency, ceilings().max_concurrency), (width, ceilings().max_width)] {
        assert_eq!(field["maximum"], ceiling, "{field}");
        assert!(field["description"].as_str().unwrap().contains(&format!("at most {ceiling},")), "{field}");
    }
    for name in [ACTION_RESUME, ACTION_STATUS, ACTION_STOP, ACTION_PROBLEMS] {
        let tool = decl.actions[&ActionName::new(name).unwrap()].tool.clone().unwrap();
        assert_eq!(tool.params["required"], json!(["run"]), "{name}");
    }
    let resume = decl.actions[&ActionName::new(ACTION_RESUME).unwrap()].tool.clone().unwrap();
    for tool in [&start, &resume] {
        assert_eq!(tool.params["properties"]["timeout_ms"]["maximum"], MAX_START_TIMEOUT_MS);
    }
    for said in ["returns at once", "reminder", "failure reasons", "pi_doubling.py", "prover-agent.j2"] {
        assert!(start.description.contains(said), "start says {said:?}: {}", start.description);
    }
    // What `lean register` enforces, and what status quotes from workers.
    let source = &start.params["properties"]["statements"]["items"]["properties"]["source"];
    assert!(source["description"].as_str().unwrap().contains("exactly `import Mathlib`"));
    let status = decl.actions[&ActionName::new(ACTION_STATUS).unwrap()].tool.clone().unwrap();
    assert!(status.description.contains("untrusted"), "{}", status.description);
    // A two-segment resource is one `Segments` argument split on `/`.
    let tree = decl.actions[&ActionName::new(ACTION_TREE).unwrap()].tool.clone().unwrap();
    assert_eq!(tree.params["required"], json!(["path"]));
    assert!(tree.params["properties"]["path"]["description"].as_str().unwrap().contains("<run>/<problem>"));
    assert!(
        tree.description.contains("cut") && tree.description.contains("truncated: true"),
        "{}",
        tree.description
    );
}

#[test]
fn problem_rows_and_trees_have_the_documented_shapes_and_bounds() {
    let queued = ProblemRow::queued("putnam_2024_a3");
    let json = serde_json::to_value(&queued).unwrap();
    assert_eq!(
        json,
        json!({"problem": "putnam_2024_a3", "state": "queued", "best": null, "rounds": 0, "nodes": 0,
               "tokens_in": 0, "tokens_out": 0, "stop": null, "solved": false}),
        "nulls are written out: every field is present on every row"
    );
    let done = ProblemRow {
        problem: "putnam_2023_a1".into(),
        state: ProblemState::Done,
        best: Some(1.0),
        rounds: 2,
        nodes: 4,
        tokens_in: 60_000,
        tokens_out: 11_000,
        stop: Some("solved".into()),
        solved: true,
    };
    let json = serde_json::to_value(&done).unwrap();
    assert_eq!(
        (json["state"].clone(), json["best"].clone(), json["stop"].clone()),
        (json!("done"), json!(1.0), json!("solved"))
    );
    let list = ProblemsResult { run: "gen2-heldout".into(), problems: vec![done.clone(), queued] };
    let text = serde_json::to_string(&list).unwrap();
    assert_eq!(serde_json::from_str::<ProblemsResult>(&text).unwrap(), list, "round trip");
    for (state, text) in
        [(ProblemState::Queued, "queued"), (ProblemState::Active, "active"), (ProblemState::Done, "done")]
    {
        assert_eq!(serde_json::to_value(state).unwrap(), Value::String(text.into()));
    }

    let empty = TreeResult::queued("gen2-heldout", "putnam_2024_a3");
    assert_eq!(
        serde_json::to_value(&empty).unwrap(),
        json!({"run": "gen2-heldout", "problem": "putnam_2024_a3", "state": "queued", "nodes": [],
               "rounds": [], "policy": null, "truncated": false})
    );
    let node = TreeNode {
        id: 3,
        parent: Some(2),
        round: 2,
        score: 1.0,
        verified: Some(true),
        check: Some(TreeCheck { status: "complete".into(), errors: 0, sorries: 0 }),
        sketch: None,
        worker: Some(TreeWorker {
            task: "t000003".into(),
            process: 41,
            model_calls: 2,
            input_tokens: 11_000,
            output_tokens: 2_600,
        }),
        excerpt: "+ have h1 …".into(),
        ms_since_start: 41_000,
    };
    let json = serde_json::to_value(&node).unwrap();
    assert_eq!(json["worker"]["task"], "t000003");
    assert_eq!(json["check"], json!({"status": "complete", "errors": 0, "sorries": 0}));
    let root =
        TreeNode { id: 0, parent: None, round: 0, verified: None, check: None, worker: None, ..node.clone() };
    let json = serde_json::to_value(&root).unwrap();
    assert!(json["parent"].is_null() && json["worker"].is_null() && json["check"].is_null(), "{json}");
    let round = TreeRound {
        round: 2,
        batch: vec![2, 2],
        children: vec![3, 4],
        failed: vec![Failed { task: "t000005".into(), why: "timed_out".into() }],
        reason: Some("solved".into()),
        policy_error: None,
        tokens_in: 60_000,
        tokens_out: 11_000,
        usd: None,
    };
    let json = serde_json::to_value(&round).unwrap();
    assert_eq!(json["failed"], json!([{"task": "t000005", "why": "timed_out"}]));
    assert!(json["policy_error"].is_null() && json.get("usd").is_none(), "{json}");
    // The byte bound: a tree of worst-case nodes (a 200-character excerpt
    // of characters JSON escapes to six bytes each, every field present)
    // and rounds with failed continuations encodes within MAX_TREE_BYTES,
    // cut to a prefix in journal order — 40 rounds of 30 nodes each.
    let worst = TreeNode { excerpt: "\u{1f}".repeat(TREE_EXCERPT_CHARS), ..node.clone() };
    let per_round = 30_u64;
    let nodes: Vec<TreeNode> = (0..1200_u64)
        .map(|id| TreeNode { id, round: (id / per_round) as u32 + 1, ..worst.clone() })
        .collect();
    let rounds: Vec<TreeRound> = (1..=40_u32).map(|r| TreeRound { round: r, ..round.clone() }).collect();
    let mut big = TreeResult {
        run: "r".into(),
        problem: "p".into(),
        state: ProblemState::Done,
        nodes: nodes.clone(),
        rounds: rounds.clone(),
        policy: Some(TreePolicy { kind: "builtin".into(), program: None }),
        truncated: false,
    };
    assert!(serde_json::to_vec(&big).unwrap().len() > MAX_TREE_BYTES, "the fixture overflows");
    big.bound();
    assert!(big.truncated);
    let encoded = serde_json::to_vec(&big).unwrap().len();
    assert!(encoded <= MAX_TREE_BYTES, "{encoded} bytes");
    assert!(!big.nodes.is_empty() && encoded > MAX_TREE_BYTES - 2 * 1024, "as much as fits: {encoded} bytes");
    assert_eq!(
        big.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        (0..big.nodes.len() as u64).collect::<Vec<_>>(),
        "a prefix in creation order"
    );
    // Journal order: a round's record comes after its nodes, so every kept
    // record's children are kept, and no record follows a cut node.
    let last_round = big.rounds.last().map_or(0, |r| r.round);
    let kept_through = big.nodes.iter().filter(|n| n.round <= last_round).count();
    assert_eq!(kept_through, nodes.iter().filter(|n| n.round <= last_round).count());
    assert!(big.nodes.last().is_some_and(|n| n.round >= last_round), "{last_round}");
    // Cut anywhere in the middle of the fixture, not at a round boundary by luck.
    assert!(big.rounds.len() < rounds.len() && big.nodes.len() < nodes.len());
    // Bounding again changes nothing: the cut is stable.
    let once = big.clone();
    big.bound();
    assert_eq!(big, once);
    let mut small = TreeResult { nodes: vec![node], rounds: vec![round], truncated: false, ..big.clone() };
    small.bound();
    assert!(!small.truncated, "a small tree is whole");
    assert_eq!(small.nodes.len() + small.rounds.len(), 2);
    let text = serde_json::to_string(&small).unwrap();
    assert_eq!(serde_json::from_str::<TreeResult>(&text).unwrap(), small, "round trip");
    assert_eq!(serde_json::to_value(&small).unwrap()["policy"], json!({"kind": "builtin", "program": null}));
}

#[test]
fn a_task_naming_both_problem_sources_parses_and_is_refused() {
    let text = r#"{
      "v": 1, "name": "gen2-heldout",
      "problems": ["putnam_1962_b2", "putnam_1985_a1"], "problems_file": "curricula/heldout.txt",
      "range": [0, 20],
      "prompt_template": "...", "prompt_template_file": "prompts/putnam.j2",
      "policy": "mine/pi_custom.py", "concurrency": 2,
      "limits": {"w": 2, "k1": 6, "calls_per_continuation": 2, "fanout": 2,
                 "model_calls_per_problem": 24, "max_tokens_per_problem": 0, "max_usd_per_problem": "20"}
    }"#;
    let task: Task = serde_json::from_str(text).unwrap();
    assert_eq!(task.range, Some([0, 20]));
    assert_eq!(refused(task.clone()), TaskError::ProblemSource);
    let text = serde_json::to_string(&task).unwrap();
    assert_eq!(serde_json::from_str::<Task>(&text).unwrap(), task, "round trip");
}

#[test]
fn a_valid_task_is_accepted_with_the_harness_defaults() {
    let accepted = task().validate(&ceilings()).unwrap();
    assert_eq!(accepted.name, "gen2-heldout");
    assert_eq!(accepted.problems, ProblemSet::Inline(vec!["putnam_1962_b2".into(), "putnam_1985_a1".into()]));
    assert_eq!(accepted.prompt_template, TemplateSource::Inline("{{ statement }}".into()));
    assert_eq!(accepted.policy.as_ref().map(ToString::to_string).as_deref(), Some("mine/pi_custom.py"));
    assert_eq!(accepted.policy.as_ref().unwrap().segments(), ["mine", "pi_custom.py"]);
    assert_eq!(accepted.concurrency, 2);
    assert_eq!(
        accepted.limits,
        Limits {
            w: 2,
            k1: 12,
            calls_per_continuation: 3,
            fanout: 3,
            model_calls_per_problem: 8,
            max_tokens_per_problem: 0,
            max_usd_micro_per_problem: 20_000_000,
        }
    );
    let without_policy = Task { policy: None, ..task() }.validate(&ceilings()).unwrap();
    assert_eq!(without_policy.policy, None);
}

#[test]
fn only_version_one_is_read_and_unknown_fields_are_refused() {
    assert_eq!(refused(Task { v: 2, ..task() }), TaskError::Version { got: 2 });
    assert_eq!(refused(Task { v: 0, ..task() }), TaskError::Version { got: 0 });
    let mut json = serde_json::to_value(task()).unwrap();
    json["output"] = json!("/tmp/x");
    assert!(serde_json::from_value::<Task>(json).is_err(), "unknown field");
    let mut json = serde_json::to_value(task()).unwrap();
    json["limits"]["price_in"] = json!("1");
    assert!(serde_json::from_value::<Task>(json).is_err(), "unknown limit (prices are not task fields)");
    let mut json = serde_json::to_value(task()).unwrap();
    json.as_object_mut().unwrap().remove("v");
    assert!(serde_json::from_value::<Task>(json).is_err(), "v is required");
}

#[test]
fn names_follow_the_pattern() {
    for ok in ["a", "0", "gen2-heldout", "a-", &"a".repeat(40)] {
        assert!(Task { name: ok.into(), ..task() }.validate(&ceilings()).is_ok(), "{ok:?}");
    }
    for bad in ["", "-a", "A", "a_b", "a/b", "a b", "ü", &"a".repeat(41)] {
        assert_eq!(refused(Task { name: bad.into(), ..task() }), TaskError::Name { name: bad.into() });
    }
}

#[test]
fn exactly_one_problem_source_is_required() {
    let neither = Task { problems: None, problems_file: None, ..task() };
    assert_eq!(refused(neither), TaskError::ProblemSource);
    let both = Task { problems_file: Some("curricula/heldout.txt".into()), ..task() };
    assert_eq!(refused(both), TaskError::ProblemSource);
    let file = Task {
        problems: None,
        problems_file: Some("curricula/heldout.txt".into()),
        range: Some([0, 20]),
        ..task()
    };
    let accepted = file.validate(&ceilings()).unwrap();
    let ProblemSet::File { path, range } = accepted.problems else { panic!("a file source") };
    assert_eq!((path.to_string(), range), ("curricula/heldout.txt".to_owned(), Some([0, 20])));
}

#[test]
fn problem_ids_are_checked_deduplicated_in_order_then_sliced() {
    let ids = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(problem_list(ids(&["b", "a", "b", "c", "a"]), None, 100).unwrap(), ["b", "a", "c"]);
    assert_eq!(problem_list(ids(&["a", "b", "c", "d"]), Some([1, 3]), 100).unwrap(), ["b", "c"]);
    assert_eq!(problem_list(ids(&["a", "b", "c"]), Some([1, 20]), 100).unwrap(), ["b", "c"], "clamped");
    assert_eq!(
        problem_list(ids(&["a", "b", "a", "c"]), Some([2, 3]), 100).unwrap(),
        ["c"],
        "range after dedup"
    );
    assert_eq!(problem_list(ids(&["a", "b"]), Some([2, 3]), 100), Err(TaskError::NoProblems));
    assert_eq!(problem_list(ids(&["a", "b"]), Some([2, 2]), 100), Err(TaskError::Range { start: 2, end: 2 }));
    assert_eq!(problem_list(ids(&["a", "b"]), Some([3, 1]), 100), Err(TaskError::Range { start: 3, end: 1 }));
    assert_eq!(problem_list(ids(&[]), None, 100), Err(TaskError::NoProblems));
    // The ceiling counts the final list: after de-duplication and the range.
    assert_eq!(
        problem_list(ids(&["a", "b", "c", "a"]), None, 2),
        Err(TaskError::TooManyProblems { got: 3, ceiling: 2 })
    );
    assert_eq!(problem_list(ids(&["a", "b", "c", "a"]), Some([1, 3]), 2).unwrap(), ["b", "c"]);
    let many: Vec<String> = (0..3).map(|i| format!("p{i}")).collect();
    let over = Task { problems: Some(many), ..task() };
    let two = Ceilings { max_problems_per_task: 2, ..ceilings() };
    assert_eq!(over.validate(&two).unwrap_err(), TaskError::TooManyProblems { got: 3, ceiling: 2 });
    assert!(
        over.validate(&two).unwrap_err().to_string().contains("3 problems; at most 2 per task"),
        "{}",
        over.validate(&two).unwrap_err()
    );
    for bad in ["", "Putnam_1962_b2", "putnam-1962", "a b", "a/b"] {
        let err = problem_list(ids(&["putnam_1962_b2", bad]), None, 100).unwrap_err();
        assert_eq!(err, TaskError::ProblemId { id: bad.into() });
    }
    // The same rules through the document.
    let t = Task { problems: Some(ids(&["b", "a", "b"])), range: Some([1, 5]), ..task() };
    assert_eq!(t.validate(&ceilings()).unwrap().problems, ProblemSet::Inline(ids(&["a"])));
    assert_eq!(refused(Task { problems: Some(vec![]), ..task() }), TaskError::NoProblems);
    assert_eq!(refused(Task { range: Some([1, 1]), ..task() }), TaskError::Range { start: 1, end: 1 });
    let file = Task { problems: None, problems_file: Some("c.txt".into()), range: Some([4, 2]), ..task() };
    assert_eq!(refused(file), TaskError::Range { start: 4, end: 2 }, "an inverted range on a file source");
    assert!(serde_json::from_str::<Task>(r#"{"v":1,"name":"x","range":[0],"concurrency":1}"#).is_err());
    assert!(serde_json::from_str::<Task>(r#"{"v":1,"name":"x","range":[-1,2],"concurrency":1}"#).is_err());
}

#[test]
fn a_problems_file_is_one_id_per_line_with_comments() {
    let text = "# heldout\nputnam_1962_b2\n\n  putnam_1985_a1  \n# putnam_1999_a1\nputnam_1962_b2\n";
    assert_eq!(problem_ids_in(text), ["putnam_1962_b2", "putnam_1985_a1", "putnam_1962_b2"]);
    assert_eq!(problem_list(problem_ids_in(text), None, 100).unwrap(), ["putnam_1962_b2", "putnam_1985_a1"]);
    assert!(problem_ids_in("").is_empty());
}

#[test]
fn exactly_one_template_source_is_required_and_inline_text_is_bounded() {
    let neither = Task { prompt_template: None, prompt_template_file: None, ..task() };
    assert_eq!(refused(neither), TaskError::TemplateSource);
    let both = Task { prompt_template_file: Some("prompts/putnam.j2".into()), ..task() };
    assert_eq!(refused(both), TaskError::TemplateSource);
    let file =
        Task { prompt_template: None, prompt_template_file: Some("prompts/putnam.j2".into()), ..task() };
    let TemplateSource::File(path) = file.validate(&ceilings()).unwrap().prompt_template else {
        panic!("a file source")
    };
    assert_eq!(path.to_string(), "prompts/putnam.j2");
    let at_the_limit = "x".repeat(MAX_PROMPT_TEMPLATE_BYTES);
    assert!(Task { prompt_template: Some(at_the_limit), ..task() }.validate(&ceilings()).is_ok());
    let over = "x".repeat(MAX_PROMPT_TEMPLATE_BYTES + 1);
    assert_eq!(
        refused(Task { prompt_template: Some(over), ..task() }),
        TaskError::TemplateTooLong { bytes: MAX_PROMPT_TEMPLATE_BYTES + 1 }
    );
    for empty in ["", " \n\t"] {
        assert_eq!(refused(Task { prompt_template: Some(empty.into()), ..task() }), TaskError::TemplateEmpty);
    }
    assert_eq!(check_prompt_template("{{ statement }}"), Ok(()));
}

#[test]
fn paths_are_relative_without_dotdot_or_empty_segments() {
    for ok in ["prompts/putnam.j2", "heldout.txt", "a/b/c", "gen2-pi_02.py"] {
        let path = RelativePath::new("policy", ok).unwrap();
        assert_eq!(path.to_string(), ok);
        assert_eq!(path.segments().join("/"), ok);
    }
    let refusals = [
        ("/etc/passwd", "must not start with /"),
        ("", "empty segment"),
        ("a//b", "empty segment"),
        ("a/", "empty segment"),
        ("..", "`..` segment"),
        ("a/../b", "`..` segment"),
        ("../b", "`..` segment"),
        ("a\\..\\b", "invalid segment"),
        ("./a", "invalid segment"),
        ("a\u{0}b", "invalid segment"),
    ];
    for (bad, why) in refusals {
        assert_eq!(
            RelativePath::new("policy", bad),
            Err(TaskError::Path { field: "policy", path: bad.into(), why }),
            "{bad:?}"
        );
    }
    // Every path field goes through the same rule, named in the refusal.
    let file = Task { problems: None, problems_file: Some("/abs".into()), ..task() };
    assert!(matches!(refused(file), TaskError::Path { field: "problems_file", .. }));
    let file = Task { prompt_template: None, prompt_template_file: Some("x/../y".into()), ..task() };
    assert!(matches!(refused(file), TaskError::Path { field: "prompt_template_file", .. }));
    assert!(matches!(
        refused(Task { policy: Some("".into()), ..task() }),
        TaskError::Path { field: "policy", .. }
    ));
}

#[test]
fn concurrency_is_between_one_and_the_ceiling() {
    assert_eq!(refused(Task { concurrency: 0, ..task() }), TaskError::Concurrency { got: 0, ceiling: 4 });
    assert_eq!(refused(Task { concurrency: 5, ..task() }), TaskError::Concurrency { got: 5, ceiling: 4 });
    assert_eq!(Task { concurrency: 4, ..task() }.validate(&ceilings()).unwrap().concurrency, 4);
    let none = Ceilings { max_concurrency: 0, ..ceilings() };
    assert!(Task { concurrency: 1, ..task() }.validate(&none).is_err(), "a zero ceiling refuses every task");
}

#[test]
fn limits_keep_the_harness_bounds() {
    let with = |limits: TaskLimits| Task { limits, ..task() }.validate(&ceilings()).map(|t| t.limits);
    for (field, limits) in [
        ("w", TaskLimits { w: Some(0), ..TaskLimits::default() }),
        ("k1", TaskLimits { k1: Some(0), ..TaskLimits::default() }),
        ("fanout", TaskLimits { fanout: Some(0), ..TaskLimits::default() }),
        ("calls_per_continuation", TaskLimits { calls_per_continuation: Some(0), ..TaskLimits::default() }),
    ] {
        assert_eq!(with(limits), Err(TaskError::LimitZero { field }));
    }
    let starved = TaskLimits { model_calls_per_problem: Some(2), ..TaskLimits::default() };
    assert_eq!(
        with(starved),
        Err(TaskError::ModelCallsBelowContinuation { model_calls: 2, calls_per_continuation: 3 })
    );
    let zero_calls = TaskLimits {
        model_calls_per_problem: Some(0),
        calls_per_continuation: Some(1),
        ..TaskLimits::default()
    };
    assert!(matches!(with(zero_calls), Err(TaskError::ModelCallsBelowContinuation { .. })));
    let full = TaskLimits {
        w: Some(1),
        k1: Some(6),
        calls_per_continuation: Some(2),
        fanout: Some(2),
        model_calls_per_problem: Some(24),
        max_tokens_per_problem: Some(500_000),
        max_usd_per_problem: Some("0.09576".into()),
    };
    assert_eq!(
        with(full).unwrap(),
        Limits {
            w: 1,
            k1: 6,
            calls_per_continuation: 2,
            fanout: 2,
            model_calls_per_problem: 24,
            max_tokens_per_problem: 500_000,
            max_usd_micro_per_problem: 95_760,
        }
    );
}

#[test]
fn the_usd_cap_is_exact_defaults_to_the_ceiling_and_never_exceeds_it() {
    let with = |usd: &str| {
        let limits = TaskLimits { max_usd_per_problem: Some(usd.into()), ..TaskLimits::default() };
        Task { limits, ..task() }.validate(&ceilings()).map(|t| t.limits.max_usd_micro_per_problem)
    };
    assert_eq!(with("20"), Ok(20_000_000), "equal to the ceiling");
    assert_eq!(with("0.000001"), Ok(1));
    assert_eq!(with("5."), Ok(5_000_000));
    assert_eq!(
        with("20.000001"),
        Err(TaskError::UsdAboveCeiling { micro: 20_000_001, ceiling_micro: 20_000_000 })
    );
    assert_eq!(with("0"), Err(TaskError::UsdAboveCeiling { micro: 0, ceiling_micro: 20_000_000 }), "no cap");
    for bad in ["", ".5", "1.2345678", "1e3", "-1", "$20", "20 ", "0x10", "NaN"] {
        assert_eq!(with(bad), Err(TaskError::Usd { value: bad.into() }), "{bad:?}");
    }
    assert_eq!(task().validate(&ceilings()).unwrap().limits.max_usd_micro_per_problem, 20_000_000, "default");
    let uncapped = Ceilings { max_usd_micro_per_problem: 0, ..ceilings() };
    assert_eq!(task().validate(&uncapped).unwrap().limits.max_usd_micro_per_problem, 0);
    let limits = TaskLimits { max_usd_per_problem: Some("1000".into()), ..TaskLimits::default() };
    assert_eq!(
        Task { limits, ..task() }.validate(&uncapped).unwrap().limits.max_usd_micro_per_problem,
        1_000_000_000,
        "no ceiling, no bound"
    );
}

#[test]
fn micro_usd_parses_exactly_and_prints_back_shortest() {
    assert_eq!(micro_usd("20"), Some(20_000_000));
    assert_eq!(micro_usd("0.09576"), Some(95_760));
    assert_eq!(micro_usd("0.000001"), Some(1));
    assert_eq!(micro_usd("18446744073709.551615"), Some(u64::MAX));
    assert_eq!(micro_usd("18446744073709.551616"), None, "overflow");
    assert_eq!(micro_usd("0.1234567"), None, "seven decimals");
    for (micro, text) in
        [(0, "0"), (1, "0.000001"), (95_760, "0.09576"), (20_000_000, "20"), (20_500_000, "20.5")]
    {
        assert_eq!(usd_string(micro), text);
        assert_eq!(micro_usd(text), Some(micro), "round trip");
    }
}

#[test]
fn the_normalized_document_revalidates_to_the_same_task_with_inline_sources() {
    let accepted = task().validate(&ceilings()).unwrap();
    let ProblemSet::Inline(problems) = accepted.problems.clone() else { panic!("inline") };
    let TemplateSource::Inline(template) = accepted.prompt_template.clone() else { panic!("inline") };
    let document = accepted.normalized(problems, template);
    assert_eq!(document.v, TASK_VERSION);
    assert_eq!(document.limits.max_usd_per_problem.as_deref(), Some("20"));
    assert_eq!(document.validate(&ceilings()).unwrap(), accepted);
    let json = serde_json::to_value(&document).unwrap();
    assert!(json.get("problems_file").is_none() && json.get("range").is_none(), "no nulls");
    assert_eq!(json["limits"]["k1"], 12);

    // A file-sourced task normalizes with what the service read.
    let file = Task { problems: None, problems_file: Some("c.txt".into()), range: Some([1, 2]), ..task() };
    let accepted = file.validate(&ceilings()).unwrap();
    let read = problem_list(problem_ids_in("a\nb\nc\n"), Some([1, 2]), 100).unwrap();
    let document = accepted.normalized(read, "{{ statement }}".into());
    let again = document.validate(&ceilings()).unwrap();
    assert_eq!(again.problems, ProblemSet::Inline(vec!["b".into()]));
    assert_eq!((again.limits, again.concurrency, again.policy), (accepted.limits, 2, accepted.policy));
}

#[test]
fn a_refusal_is_an_invalid_input_error() {
    let error: Error = refused(Task { v: 3, ..task() }).into();
    assert_eq!(error.code, ErrorCode::InvalidInput);
    assert!(error.message.contains("task version 3"), "{}", error.message);
    let no_cap = TaskError::UsdAboveCeiling { micro: 0, ceiling_micro: 20_000_000 }.to_string();
    assert!(no_cap.contains("no cap") && no_cap.contains("20"), "{no_cap}");
}

#[test]
fn run_records_have_the_documented_shapes() {
    let status = RunStatus {
        run: "gen2-heldout".into(),
        state: RunState::Running,
        problems: 20,
        finished: 3,
        solved: 1,
        active: vec!["putnam_1962_b2".into(), "putnam_1985_a1".into()],
        tokens_in: 1000,
        tokens_out: 200,
        usd: None,
        error: None,
        task: TaskShape {
            policy: Some("mine/pi_custom.py".into()),
            concurrency: 2,
            limits: TaskLimits::default(),
            worker_network: None,
        },
        failed_continuations: 0,
        last_failure: None,
        started_at_ms: Some(1_758_240_060_000),
        failure_reasons: vec![],
        other_failures: 0,
    };
    let json = serde_json::to_value(&status).unwrap();
    assert!(json.get("failure_reasons").is_none() && json.get("other_failures").is_none());
    assert_eq!(json["state"], "running");
    assert_eq!(json["failed_continuations"], 0);
    assert!(json.get("last_failure").is_none(), "absent until a continuation fails");
    assert_eq!(json["task"]["concurrency"], 2);
    assert_eq!(json["task"]["policy"], "mine/pi_custom.py");
    assert_eq!(json["started_at_ms"], 1_758_240_060_000_u64);
    assert!(json.get("usd").is_none() && json.get("error").is_none(), "skipped when absent");
    assert_eq!(serde_json::from_value::<RunStatus>(json).unwrap(), status);
    let priced =
        RunStatus { usd: Some(0.25), state: RunState::Failed, error: Some("boom".into()), ..status.clone() };
    let json = serde_json::to_value(&priced).unwrap();
    assert_eq!(
        (json["usd"].clone(), json["state"].clone(), json["error"].clone()),
        (json!(0.25), json!("failed"), json!("boom"))
    );

    let summary = RunSummary::of("default", &status);
    let json = serde_json::to_value(&summary).unwrap();
    assert!(json.get("active").is_none(), "a summary has no active list");
    assert_eq!(
        (json["project"].clone(), json["run"].clone(), json["solved"].clone()),
        (json!("default"), json!("gen2-heldout"), json!(1))
    );
    assert_eq!(serde_json::from_value::<RunSummary>(json).unwrap(), summary);
    let list = ListResult { runs: vec![summary], next: None };
    assert_eq!(serde_json::to_value(&list).unwrap()["runs"].as_array().unwrap().len(), 1);

    for (state, text) in [
        (RunState::Queued, "queued"),
        (RunState::Running, "running"),
        (RunState::Done, "done"),
        (RunState::Stopped, "stopped"),
        (RunState::Failed, "failed"),
    ] {
        assert_eq!(serde_json::to_value(state).unwrap(), Value::String(text.into()));
    }
    let failing = RunStatus {
        failure_reasons: vec![FailureReason {
            kind: FailureKind::Preflight,
            reason: failure_reason("statement \"kmp\": Lean says: unknown constant"),
            count: 2,
            problems: vec!["kmp".into()],
            last: failure_excerpt("statement \"kmp\": Lean says:\nunknown constant 'kmp'"),
        }],
        other_failures: 3,
        ..status.clone()
    };
    let json = serde_json::to_value(&failing).unwrap();
    assert_eq!(json["failure_reasons"][0]["kind"], "preflight");
    assert_eq!(json["other_failures"], 3);
    assert_eq!(serde_json::from_value::<RunStatus>(json).unwrap(), failing);

    let start = StartResult { run: "gen2-heldout".into(), problems: 20, queued_ahead: 1 };
    let json = serde_json::to_string(&start).unwrap();
    assert_eq!(json, r#"{"run":"gen2-heldout","problems":20,"queued_ahead":1}"#);
    assert_eq!(serde_json::from_str::<StartResult>(&json).unwrap(), start);
}

#[test]
fn the_width_ceiling_refuses_a_wider_round_and_names_itself() {
    let with = |w: usize| Task { limits: TaskLimits { w: Some(w), ..TaskLimits::default() }, ..task() };
    assert_eq!(with(16).validate(&ceilings()).unwrap().limits.w, 16);
    let error = refused(with(17));
    assert_eq!(error, TaskError::Width { got: 17, ceiling: 16 });
    assert!(error.to_string().contains("at most 16"), "{error}");
    let narrow = Ceilings { max_width: 1, ..ceilings() };
    assert_eq!(
        task().validate(&narrow).unwrap_err(),
        TaskError::Width { got: Limits::DEFAULT_W, ceiling: 1 },
        "the default width is checked too"
    );
}

#[test]
fn start_and_resume_timeouts_are_bounded_and_not_recorded() {
    let with = |ms: u64| Task { timeout_ms: Some(ms), ..task() };
    assert!(with(MAX_START_TIMEOUT_MS).validate(&ceilings()).is_ok());
    for bad in [0, MAX_START_TIMEOUT_MS + 1] {
        assert_eq!(refused(with(bad)), TaskError::Timeout { got: bad });
        assert!(ResumeRequest { timeout_ms: Some(bad) }.validate().is_err());
    }
    let accepted = with(300_000).validate(&ceilings()).unwrap();
    let ProblemSet::Inline(problems) = accepted.problems.clone() else { panic!("inline") };
    let document = accepted.normalized(problems, "{{ statement }}".into());
    assert_eq!(document.timeout_ms, None, "a property of the call, not of the run");
    assert!(serde_json::to_value(&document).unwrap().get("timeout_ms").is_none());
    let resume: ResumeRequest = serde_json::from_value(json!({})).unwrap();
    assert_eq!(resume.timeout_ms, None);
    assert!(resume.validate().is_ok());
    assert!(serde_json::from_value::<ResumeRequest>(json!({"timeout_ms": 1, "task": {}})).is_err());
}

#[test]
fn failure_reasons_group_by_a_bounded_first_line_without_digits_or_control_characters() {
    assert_eq!(
        failure_reason("exited_without_submitting: t000123 timed out after 300000 ms\nmore"),
        "exited_without_submitting: tN timed out after N ms"
    );
    assert_eq!(
        failure_reason("  \n lean check: 3:14: unknown identifier 'x'"),
        failure_reason("lean check: 12:7: unknown identifier 'x'")
    );
    assert_eq!(failure_reason("model_error:\u{1b}[31mhttp 402\u{7}"), "model_error: [Nmhttp N");
    assert_eq!(failure_reason(&"é".repeat(1000)).chars().count(), MAX_FAILURE_REASON_CHARS);
    assert_eq!(failure_reason(""), "");
    let last = failure_excerpt(&format!("line\n\tindented\u{1b}[0m{}", "x".repeat(1000)));
    assert!(last.starts_with("line\n\tindented[0m") && !last.contains('\u{1b}'), "{last}");
    assert_eq!(last.chars().count(), MAX_FAILURE_LAST_CHARS);
}

/// What an agent is told of the workers' network is the value it was given,
/// for either value, no value when there is none to give, and whether a task
/// may choose.
#[test]
fn the_environment_states_the_workers_network_it_was_given() {
    assert!(environment(Some(true), true).contains("workers' shell has network access"));
    assert!(environment(Some(false), true).contains("workers' shell has no IP network"));
    let unsaid = environment(None, false);
    assert!(!unsaid.contains("has no IP network") && !unsaid.contains("has network access"), "{unsaid}");
    assert!(environment(Some(false), true).contains("chooses for its run with `worker_network`"));
    assert!(unsaid.contains("`worker_network: true` is refused"));
}
