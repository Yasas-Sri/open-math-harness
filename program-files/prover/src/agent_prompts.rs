//! Tool-loop language is task input, rendered once before the first model call.
//! Trigger conditions and enforcement remain in the agent. There is no built-in
//! text fallback, and rendered messages use the ordinary transcript path.

use nucleus_explore_interface as explore;
use nucleus_protocol::ToolInfo;
use nucleus_sdk::tool_name;
use serde::Serialize;

use crate::{PROMPT_TEMPLATE_FAILURE, PromptTemplate, PromptVars};

#[derive(Debug)]
pub(crate) struct AgentPrompts {
    pub nudge: String,
    pub continue_after_length: String,
    pub batch_hint: String,
    pub seed_hint: String,
    /// The user message after a context reset.
    pub context_reset: String,
    pub collection: Option<CollectionPrompts>,
    /// Present when this worker holds both plan tools.
    pub planning: Option<PlanningPrompts>,
}

#[derive(Debug)]
pub(crate) struct CollectionPrompts {
    pub instruction: String,
    pub nudge: String,
    pub tool_description: String,
}

/// The plan tools' listed names and the three planning messages.
#[derive(Debug)]
pub(crate) struct PlanningPrompts {
    pub publish_tool: String,
    pub read_tool: String,
    pub publish: String,
    pub read: String,
    pub review: String,
}

#[derive(Serialize)]
struct RuntimeVars<'a, 'b> {
    #[serde(flatten)]
    base: &'a PromptVars<'b>,
    collection_calls: u64,
    submit_tool: Option<&'a str>,
}

impl AgentPrompts {
    /// Renders every runtime mode. The submit and plan tool names are the
    /// kernel's listing on `explore_endpoint`, whatever `vars` says; one
    /// plan tool without the other is a template failure, because planning
    /// would then be half enforced.
    pub fn render(
        template: &PromptTemplate,
        vars: &PromptVars<'_>,
        tools: &[ToolInfo],
        explore_endpoint: &str,
    ) -> Result<Self, String> {
        let listed = |action: &str| listed_tool(tools, explore_endpoint, action);
        let submit_tool = listed(explore::ACTION_SUBMIT);
        let publish_plan_tool = listed(explore::ACTION_PUBLISH_PLAN);
        let read_plans_tool = listed(explore::ACTION_READ_PLANS);
        if publish_plan_tool.is_some() != read_plans_tool.is_some() {
            return Err(format!(
                "{PROMPT_TEMPLATE_FAILURE}: plan coordination requires both {} and {} on endpoint \
                 {explore_endpoint:?}",
                explore::ACTION_PUBLISH_PLAN,
                explore::ACTION_READ_PLANS
            ));
        }
        let render = |mode| {
            let base = PromptVars {
                mode,
                publish_plan_tool: publish_plan_tool.as_deref(),
                read_plans_tool: read_plans_tool.as_deref(),
                ..vars.clone()
            };
            let context = RuntimeVars {
                base: &base,
                collection_calls: explore::COLLECTION_CALLS,
                submit_tool: submit_tool.as_deref(),
            };
            let text = template
                .render_context(&context)
                .map_err(|why| format!("{PROMPT_TEMPLATE_FAILURE} mode {mode:?}: {why}"))?;
            if text.trim().is_empty() {
                return Err(format!("{PROMPT_TEMPLATE_FAILURE} rendered nothing for mode {mode:?}"));
            }
            Ok(text.trim().to_owned())
        };
        Ok(Self {
            nudge: render("nudge")?,
            continue_after_length: render("continue_after_length")?,
            batch_hint: render("batch_hint")?,
            seed_hint: render("seed_hint")?,
            context_reset: render("context_reset")?,
            collection: if submit_tool.is_some() {
                Some(CollectionPrompts {
                    instruction: render("collection_instruction")?,
                    nudge: render("collection_nudge")?,
                    tool_description: render("collection_tool_description")?,
                })
            } else {
                None
            },
            planning: match (&publish_plan_tool, &read_plans_tool) {
                (Some(publish_tool), Some(read_tool)) => Some(PlanningPrompts {
                    publish_tool: publish_tool.clone(),
                    read_tool: read_tool.clone(),
                    publish: render("plan_publish")?,
                    read: render("plan_read")?,
                    review: render("plan_review")?,
                }),
                _ => None,
            },
        })
    }
}

/// The kernel's name for `action` on `endpoint`, when this worker holds it.
pub(crate) fn listed_tool(tools: &[ToolInfo], endpoint: &str, action: &str) -> Option<String> {
    tools
        .iter()
        .find(|tool| tool.endpoint.as_str() == endpoint && tool.action.as_str() == action)
        .map(|tool| tool_name(&tool.endpoint, &tool.action))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use nucleus_protocol::{ActionName, EffectClass, EndpointName, ResourceFrom, ResourcePath};

    const SOURCE: &str = include_str!("../prompts/prover-agent.j2");

    fn vars() -> PromptVars<'static> {
        PromptVars {
            mode: "system",
            problem: "p",
            statement: "theorem p : True := sorry",
            file: "theorem p : True := sorry",
            parent: None,
            inherited: false,
            check: None,
            siblings: &[],
            siblings_total: 0,
            branch_references: &[],
            calls_left: 10,
            attempt: 1,
            file_name: "p.lean",
            services: &[],
            publish_plan_tool: None,
            read_plans_tool: None,
            exec_tool: None,
            bash_timeout_ms: None,
        }
    }

    fn listed(endpoint: &str, action: &str) -> ToolInfo {
        ToolInfo {
            endpoint: EndpointName::new(endpoint).unwrap(),
            action: ActionName::new(action).unwrap(),
            resource: ResourcePath::new(["p"]).unwrap(),
            effect: EffectClass::Observation,
            description: String::new(),
            params: serde_json::json!({}),
            resource_from: ResourceFrom::Handle,
            endpoint_description: String::new(),
        }
    }

    fn submit() -> ToolInfo {
        listed("custom_explore", explore::ACTION_SUBMIT)
    }

    fn planning_tools() -> [ToolInfo; 2] {
        [
            listed("custom_explore", explore::ACTION_PUBLISH_PLAN),
            listed("custom_explore", explore::ACTION_READ_PLANS),
        ]
    }

    #[test]
    fn runtime_modes_render_the_actual_submission_tool_and_budget() {
        let template = PromptTemplate::new(SOURCE).unwrap();
        let prompts = AgentPrompts::render(&template, &vars(), &[submit()], "custom_explore").unwrap();
        assert_eq!(prompts.continue_after_length, "Continue");
        assert!(
            prompts.context_reset.starts_with("[context reset]")
                && prompts.context_reset.contains("`p.lean`")
        );
        let collection = prompts.collection.unwrap();
        assert!(collection.instruction.contains("`custom_explore_submit`"));
        assert!(collection.instruction.contains(&format!("{} extra model calls", explore::COLLECTION_CALLS)));
        assert!(collection.instruction.contains("`p.lean`"));
        assert!(
            AgentPrompts::render(&template, &vars(), &[], "custom_explore").unwrap().collection.is_none()
        );
    }

    #[test]
    fn missing_or_invalid_runtime_modes_fail_before_the_loop() {
        // Every mode renders its own name except `mode`, which renders `body`.
        let with = |mode: &str, body: &str| {
            let source =
                format!("{{% if mode == \"{mode}\" %}}{body}{{% else %}}{{{{ mode }}}}{{% endif %}}");
            let template = PromptTemplate::new(&source).unwrap();
            AgentPrompts::render(&template, &vars(), &[submit()], "custom_explore")
        };
        assert!(with("context_reset", "reset").is_ok());
        for (mode, body) in [
            ("continue_after_length", ""),
            ("continue_after_length", "{{ missing_variable }}"),
            ("context_reset", " \n "),
            ("collection_instruction", "{{ missing_variable }}"),
        ] {
            let why = with(mode, body).unwrap_err();
            assert!(why.contains(PROMPT_TEMPLATE_FAILURE) && why.contains(mode), "{why}");
        }
    }

    /// The plan tools the prompts name are the ones the kernel listed, never
    /// names a caller put in the variables, and planning needs both.
    #[test]
    fn planning_uses_the_listed_endpoint_and_rejects_incomplete_tool_pairs() {
        let template = PromptTemplate::new(SOURCE).unwrap();
        let tools = planning_tools();
        let forged = PromptVars {
            publish_plan_tool: Some("not_a_capability"),
            read_plans_tool: Some("another"),
            ..vars()
        };
        let planning =
            AgentPrompts::render(&template, &forged, &tools, "custom_explore").unwrap().planning.unwrap();
        assert_eq!(planning.publish_tool, "custom_explore_publish_plan");
        assert_eq!(planning.read_tool, "custom_explore_read_plans");
        assert!(planning.publish.contains("`custom_explore_publish_plan`"));
        assert!(planning.read.contains("`custom_explore_read_plans`"));
        assert!(planning.review.contains("`custom_explore_publish_plan`"));
        assert!(!planning.publish.contains("not_a_capability"));
        assert!(AgentPrompts::render(&template, &vars(), &[], "custom_explore").unwrap().planning.is_none());
        assert!(AgentPrompts::render(&template, &vars(), &tools, "different").unwrap().planning.is_none());
        for incomplete in [&tools[..1], &tools[1..]] {
            let why = AgentPrompts::render(&template, &vars(), incomplete, "custom_explore").unwrap_err();
            assert!(why.contains("requires both publish_plan and read_plans"), "{why}");
        }
        let mismatched = [tools[0].clone(), listed("different", explore::ACTION_READ_PLANS)];
        assert!(AgentPrompts::render(&template, &vars(), &mismatched, "custom_explore").is_err());
        let coordinated = PromptVars {
            publish_plan_tool: Some("custom_explore_publish_plan"),
            read_plans_tool: Some("custom_explore_read_plans"),
            ..vars()
        };
        let system = template.render(&coordinated).unwrap();
        assert!(system.contains("Before using other tools, publish a short provisional research plan"));
        assert!(!template.render(&vars()).unwrap().contains("research plan"));
    }

    /// The command limit names the command tool this worker is listed, which
    /// is not `shell_exec` when its shell has another endpoint (discovery
    /// workers in the VM use `prover-shell`), and the limit a call gets.
    #[test]
    fn the_command_limit_names_the_listed_exec_tool() {
        let template = PromptTemplate::new(SOURCE).unwrap();
        let listed =
            PromptVars { exec_tool: Some("prover-shell_exec"), bash_timeout_ms: Some(240_000), ..vars() };
        let system = template.render(&listed).unwrap();
        let limit = system
            .lines()
            .find(|line| line.contains("killed after"))
            .unwrap_or_else(|| panic!("the limit is stated: {system}"));
        assert!(limit.contains("`prover-shell_exec`") && limit.contains("240 s"), "{limit}");
        assert!(!limit.contains("`shell_exec`") && !limit.contains("`shell`"), "{limit}");
        assert!(!template.render(&vars()).unwrap().contains("killed after"), "the fixed loop states none");
    }

    /// Branch references reach the model in the `resume` message only, so
    /// they never change the cached system prefix, and the reset notice
    /// points at the files instead of repeating them.
    #[test]
    fn branch_index_is_optional_resume_context_and_does_not_change_the_system_prefix() {
        let template = PromptTemplate::new(SOURCE).unwrap();
        let references = [explore::BranchReference {
            node: 42,
            task: "t000042".into(),
            check_status: nucleus_lean_interface::CheckStatus::Sorries,
            errors: 0,
            sorries: 1,
            verified: None,
            state_file: "../t000042/STATE.md".into(),
            proof_file: "../t000042/p.lean".into(),
        }];
        let plain = vars();
        let indexed = PromptVars { branch_references: &references, ..plain.clone() };
        assert_eq!(template.render(&plain).unwrap(), template.render(&indexed).unwrap());
        let resume = template.render(&PromptVars { mode: "resume", ..indexed.clone() }).unwrap();
        assert!(resume.contains("node 42, task t000042: check sorries"), "{resume}");
        assert!(resume.contains("verified: not attempted"), "{resume}");
        assert!(resume.contains("../t000042/STATE.md") && resume.contains("../t000042/p.lean"));
        let reset = template.render(&PromptVars { mode: "context_reset", ..indexed }).unwrap();
        assert!(reset.contains("STATE.md") && reset.contains("BRANCHES.json") && !reset.contains("t000042"));
        assert!(
            !template.render(&PromptVars { mode: "resume", ..plain }).unwrap().contains("completed branches")
        );
    }
}
