//! Model-facing aliases for already-authorized service instances.
//!
//! Bindings are presentation and address translation, never grants. A catalog
//! is a snapshot of `Connection::tools`; dispatch still needs a currently held
//! handle and the kernel still checks its lifetime, generation and authority.
//!
//! When a tool has several bound instances, its schema gains an `env`
//! selector (and `workspace`, for bindings that have one), which is removed
//! before the arguments reach the provider; with one instance nothing is
//! added. A provider schema that has its own `env` or `workspace`, a
//! reference, or a root-level combinator or constraint is nested whole under
//! `input` (`{"env":"compute","input":{"env":"provider-value"}}` passes
//! `{"env":"provider-value"}`), so its parameter names keep their meaning.
//! Nesting relocates local JSON Pointer `$ref` and `$dynamicRef` in the
//! standard schema positions and keeps `$id` resource boundaries; it fetches
//! no external schema, does not validate arguments against the schema, and
//! does not relocate Draft 4 `id` or `$recursiveRef`.

use std::collections::{BTreeMap, BTreeSet};

use nucleus_protocol::{EndpointName, Error, ErrorCode, ResourceFrom, ResourcePath, ToolInfo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ToolCall, build_call, tool_name};

/// Operator-supplied alias for one environment and optional workspace.
/// `resource` is the provider's resource prefix, not an operating-system path.
/// A label mounts nothing: tools with equal `env` and `workspace` labels share
/// files only because the composition gives them the same directory, as its
/// binding descriptions tell the model (deploy/vm/nucleus.json).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBinding {
    pub service: EndpointName,
    pub endpoint: EndpointName,
    pub env: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub resource: ResourcePath,
    #[serde(default)]
    pub description: String,
}

/// One callable tool after environment aliases have been applied.
#[derive(Clone, Debug, PartialEq)]
pub struct PresentedTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug)]
struct Instance {
    binding: ToolBinding,
    tool: ToolInfo,
    scopes: Vec<ResourcePath>,
}

#[derive(Clone, Debug)]
struct BoundTool {
    service: EndpointName,
    instances: Vec<Instance>,
}

/// A model's tool presentation and routing table, derived only from its tools.
#[derive(Clone, Debug)]
pub struct ToolCatalog {
    presented: Vec<PresentedTool>,
    unbound: Vec<ToolInfo>,
    bound: BTreeMap<String, BoundTool>,
    context: String,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

/// Checks configuration independently of which providers are currently live.
/// Equal selectors are ambiguous even when one endpoint is currently absent.
pub fn validate_bindings(bindings: &[ToolBinding]) -> Result<(), Error> {
    let mut selectors = BTreeSet::new();
    for binding in bindings {
        for (field, value) in std::iter::once(("env", binding.env.as_str()))
            .chain(binding.workspace.as_deref().map(|value| ("workspace", value)))
        {
            // The labels reach the model and, in tool calls, the person:
            // neither may read one that the other does not see.
            if value.trim().is_empty() || nucleus_protocol::text::marked(value).any(|(_, hidden)| hidden) {
                return Err(invalid(format!(
                    "tool binding {field} must be nonempty and contain no hidden characters"
                )));
            }
        }
        if !selectors.insert((&binding.service, &binding.env, &binding.workspace)) {
            return Err(invalid(format!("duplicate tool binding selector for {}", binding.service)));
        }
    }
    Ok(())
}

impl ToolCatalog {
    pub fn new(tools: &[ToolInfo], bindings: &[ToolBinding]) -> Result<Self, Error> {
        validate_bindings(bindings)?;
        let endpoints: BTreeSet<_> = bindings.iter().map(|binding| &binding.endpoint).collect();
        // A bound endpoint never retains an unqualified alias, including when
        // its configured resource has no intersection with the caller's grants.
        let unbound: Vec<_> =
            tools.iter().filter(|tool| !endpoints.contains(&tool.endpoint)).cloned().collect();
        let mut bound: BTreeMap<String, BoundTool> = BTreeMap::new();
        for binding in bindings {
            let mut actions: BTreeMap<_, Instance> = BTreeMap::new();
            for tool in tools.iter().filter(|tool| tool.endpoint == binding.endpoint) {
                let reachable = match tool.resource_from {
                    ResourceFrom::Handle => binding.resource.is_within(&tool.resource),
                    _ => {
                        binding.resource.is_within(&tool.resource)
                            || tool.resource.is_within(&binding.resource)
                    }
                };
                if !reachable {
                    continue;
                }
                validate_schema(tool)?;
                if let Some(instance) = actions.get_mut(&tool.action) {
                    compatible(&instance.tool, tool)?;
                    instance.scopes.push(tool.resource.clone());
                } else {
                    actions.insert(
                        tool.action.clone(),
                        Instance {
                            binding: binding.clone(),
                            tool: tool.clone(),
                            scopes: vec![tool.resource.clone()],
                        },
                    );
                }
            }
            for (_, instance) in actions {
                let name = tool_name(&binding.service, &instance.tool.action);
                if unbound.iter().any(|tool| tool_name(&tool.endpoint, &tool.action) == name) {
                    return Err(invalid(format!("environment tool {name} conflicts with an unbound tool")));
                }
                if let Some(group) = bound.get_mut(&name) {
                    if group.service != binding.service
                        || group.instances[0].tool.action != instance.tool.action
                    {
                        return Err(invalid(format!("ambiguous environment tool name {name}")));
                    }
                    compatible(&group.instances[0].tool, &instance.tool)?;
                    group.instances.push(instance);
                } else {
                    bound.insert(
                        name,
                        BoundTool { service: binding.service.clone(), instances: vec![instance] },
                    );
                }
            }
        }
        // Multiple handles can authorize the same provider action on disjoint
        // paths (for example the memory index and summary). Keep all routing
        // scopes, but advertise one function schema per endpoint/action.
        let mut unique_unbound: BTreeMap<String, &ToolInfo> = BTreeMap::new();
        for tool in &unbound {
            let name = tool_name(&tool.endpoint, &tool.action);
            if let Some(previous) = unique_unbound.get(&name) {
                if previous.endpoint != tool.endpoint || previous.action != tool.action {
                    return Err(invalid(format!("ambiguous unbound tool name {name}")));
                }
                compatible(previous, tool)?;
            } else {
                unique_unbound.insert(name, tool);
            }
        }
        let mut presented: Vec<_> = unique_unbound
            .values()
            .map(|tool| PresentedTool {
                name: tool_name(&tool.endpoint, &tool.action),
                description: tool.description.clone(),
                parameters: tool.params.clone(),
            })
            .collect();
        for (name, group) in &bound {
            presented.push(PresentedTool {
                name: name.clone(),
                description: descriptions(group.instances.iter().map(|instance| {
                    let single = group.instances.len() == 1;
                    let input = match (needs_input(&instance.tool), single) {
                        (false, _) => "",
                        (true, true) => " Provider arguments belong in input.",
                        (true, false) => {
                            " Provider arguments belong in input; outer env/workspace select the instance."
                        }
                    };
                    if single {
                        format!("{}{input}", instance.tool.description)
                    } else {
                        format!("{}: {}{input}", instance.selector(), instance.tool.description)
                    }
                })),
                parameters: group.parameters()?,
            });
        }
        let context = context(&unbound, &bound);
        Ok(Self { presented, unbound, bound, context })
    }

    pub fn tools(&self) -> &[PresentedTool] {
        &self.presented
    }

    /// Resolves only an explicit, available environment/workspace pair, or the
    /// one instance of a tool that has only one. No missing provider or
    /// rejected selector falls back to another instance.
    /// Unbound tools reject selectors their own schema did not declare, so an
    /// attempted environment selection cannot silently invoke a local tool.
    /// `None` means the name was not advertised by this catalog.
    pub fn resolve(&self, name: &str, args: &Value) -> Option<Result<ToolCall, Error>> {
        match self.bound.get(name) {
            Some(group) => Some(group.resolve(args)),
            None => {
                let tool =
                    self.unbound.iter().find(|tool| tool_name(&tool.endpoint, &tool.action) == name)?;
                for selector in ["env", "workspace"] {
                    if args.get(selector).is_some()
                        && !tool
                            .params
                            .get("properties")
                            .and_then(Value::as_object)
                            .is_some_and(|properties| properties.contains_key(selector))
                    {
                        return Some(Err(invalid(format!(
                            "unbound tool {name} does not declare {selector}"
                        ))));
                    }
                }
                Some(build_call(tool, args))
            }
        }
    }

    /// Describes only advertised instances; unavailable/ungranted endpoints
    /// and their operator descriptions are never added to model context.
    pub fn context(&self) -> String {
        self.context.clone()
    }
}

fn descriptions(items: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut unique = Vec::<String>::new();
    for item in items {
        let item = item.as_ref();
        if !item.is_empty() && !unique.iter().any(|old| old == item) {
            unique.push(item.to_owned());
        }
    }
    unique.join("\n\n")
}

fn context(unbound: &[ToolInfo], bound: &BTreeMap<String, BoundTool>) -> String {
    // Several actions and narrowed handles can describe the same provider.
    // Preserve its full description once per selected environment/workspace.
    let mut endpoints: BTreeMap<_, Vec<&str>> = BTreeMap::new();
    for tool in unbound {
        endpoints.entry(&tool.endpoint).or_default().push(tool.endpoint_description.as_str());
    }
    let mut parts: Vec<_> = endpoints
        .into_iter()
        .filter_map(|(endpoint, descriptions_for_endpoint)| {
            let text = descriptions(descriptions_for_endpoint);
            (!text.is_empty()).then(|| format!("Endpoint {endpoint}:\n{text}"))
        })
        .collect();
    let mut instances: BTreeMap<_, Vec<String>> = BTreeMap::new();
    for instance in bound.values().flat_map(|group| &group.instances) {
        let binding = &instance.binding;
        let text = instances.entry((&binding.endpoint, &binding.env, &binding.workspace)).or_default();
        text.push(binding.description.clone());
        text.push(instance.tool.endpoint_description.clone());
    }
    for ((endpoint, env, workspace), texts) in instances {
        let selector = selector(env, workspace.as_deref());
        parts.push(format!("{selector}; endpoint={endpoint}:\n{}", descriptions(texts)));
    }
    parts.join("\n\n")
}

fn selector(env: &str, workspace: Option<&str>) -> String {
    match workspace {
        Some(workspace) => format!("env={env:?}, workspace={workspace:?}"),
        None => format!("env={env:?} (omit workspace)"),
    }
}

fn validate_schema(tool: &ToolInfo) -> Result<(), Error> {
    let schema =
        tool.params.as_object().ok_or_else(|| invalid("environment tool schema must be an object"))?;
    if schema.get("type") != Some(&json!("object")) {
        return Err(invalid("environment tool schema must declare type object"));
    }
    if let Some(properties) = schema.get("properties") {
        properties.as_object().ok_or_else(|| invalid("tool properties must be an object"))?;
    }
    if let Some(required) = schema.get("required") {
        let required = required.as_array().ok_or_else(|| invalid("tool required must be an array"))?;
        if required.iter().any(|value| value.as_str().is_none()) {
            return Err(invalid("invalid provider required parameter"));
        }
    }
    Ok(())
}

/// Whether a bound tool's own arguments move under an `input` property: when
/// its schema declares or requires `env` or `workspace`, takes its resource
/// from one of them, or needs its original root (a reference, a combinator, a
/// constraint on its properties). The outer `env` and `workspace` then keep
/// their routing meaning, and `input` becomes the provider's arguments.
fn needs_input(tool: &ToolInfo) -> bool {
    ["env", "workspace"].iter().any(|name| {
        tool.params
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key(*name))
            || tool
                .params
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.iter().any(|value| value.as_str() == Some(name)))
    }) || matches!(&tool.resource_from, ResourceFrom::Segments { param } | ResourceFrom::Host { param } if param == "env" || param == "workspace")
        // Injecting selectors into the original root would also change recursive
        // input nodes, root combinators and constraints on property names/count.
        || schema_has_reference(&tool.params)
        || ["allOf", "anyOf", "oneOf", "not", "if", "then", "else", "dependencies",
            "dependentRequired", "dependentSchemas", "patternProperties", "propertyNames",
            "unevaluatedProperties", "minProperties", "maxProperties", "const", "enum"]
            .iter().any(|key| tool.params.get(key).is_some())
}

const SCHEMA_MAPS: &[&str] =
    &["properties", "patternProperties", "$defs", "definitions", "dependentSchemas", "dependencies"];
const SCHEMA_ARRAYS: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];
const SCHEMA_VALUES: &[&str] = &[
    "not",
    "if",
    "then",
    "else",
    "additionalProperties",
    "unevaluatedProperties",
    "contains",
    "propertyNames",
    "additionalItems",
    "unevaluatedItems",
    "contentSchema",
    "items",
];

fn schema_has_reference(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else { return false };
    if schema.contains_key("$ref") || schema.contains_key("$dynamicRef") {
        return true;
    }
    schema.iter().any(|(keyword, value)| {
        if SCHEMA_MAPS.contains(&keyword.as_str()) {
            value.as_object().is_some_and(|children| children.values().any(schema_has_reference))
        } else if SCHEMA_ARRAYS.contains(&keyword.as_str()) || (keyword == "items" && value.is_array()) {
            value.as_array().is_some_and(|children| children.iter().any(schema_has_reference))
        } else {
            SCHEMA_VALUES.contains(&keyword.as_str()) && schema_has_reference(value)
        }
    })
}

fn compatible(first: &ToolInfo, next: &ToolInfo) -> Result<(), Error> {
    if first.params != next.params || first.resource_from != next.resource_from || first.effect != next.effect
    {
        return Err(invalid(format!("incompatible provider schemas for action {}", first.action)));
    }
    Ok(())
}

/// Relocates references to an anonymous schema's root when embedding it at a
/// JSON Pointer in a larger schema. Identified schema resources retain their
/// own base URI and references, including nested resources with relative $id.
/// Named anchors and external references do not depend on the embedding path.
fn relocate_schema(schema: &Value, pointer: &str) -> Value {
    let mut relocated = schema.clone();
    relocate_schema_refs(&mut relocated, pointer);
    relocated
}

fn relocate_schema_refs(schema: &mut Value, pointer: &str) {
    let Some(schema) = schema.as_object_mut() else { return };
    if schema
        .get("$id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.split('#').next().unwrap_or("").is_empty())
    {
        return;
    }
    for keyword in ["$ref", "$dynamicRef"] {
        if let Some(Value::String(reference)) = schema.get_mut(keyword) {
            let fragment = reference.strip_prefix('#').or_else(|| reference.is_empty().then_some(""));
            if let Some(fragment) = fragment
                && (fragment.is_empty()
                    || fragment.starts_with('/')
                    || fragment.get(..3).is_some_and(|prefix| prefix.eq_ignore_ascii_case("%2f")))
            {
                *reference = format!("#{pointer}{fragment}");
            }
        }
    }
    // Visit schema-bearing keywords, never arbitrary JSON. In particular a
    // property named "$ref" and a literal default/example are not references.
    for keyword in SCHEMA_MAPS {
        if let Some(Value::Object(entries)) = schema.get_mut(*keyword) {
            for child in entries.values_mut() {
                relocate_schema_refs(child, pointer);
            }
        }
    }
    for keyword in SCHEMA_ARRAYS {
        if let Some(Value::Array(children)) = schema.get_mut(*keyword) {
            for child in children {
                relocate_schema_refs(child, pointer);
            }
        }
    }
    for keyword in SCHEMA_VALUES {
        if let Some(child) = schema.get_mut(*keyword) {
            if *keyword == "items"
                && let Value::Array(children) = child
            {
                for child in children {
                    relocate_schema_refs(child, pointer);
                }
            } else {
                relocate_schema_refs(child, pointer);
            }
        }
    }
}

impl Instance {
    fn selector(&self) -> String {
        selector(&self.binding.env, self.binding.workspace.as_deref())
    }

    fn resolve(&self, mut args: serde_json::Map<String, Value>) -> Result<ToolCall, Error> {
        args.remove("env");
        args.remove("workspace");
        if needs_input(&self.tool) {
            // Only the outer fields are routing selectors. The provider's
            // original env/workspace fields retain their ordinary semantics.
            if args.len() != 1 || !args.contains_key("input") {
                return Err(invalid("wrapped tool requires input and accepts no flat provider arguments"));
            }
            args = match args.remove("input") {
                Some(Value::Object(input)) => input,
                _ => return Err(invalid("wrapped tool input must be an object")),
            };
        }
        let mut tool = self.tool.clone();
        tool.resource = self.binding.resource.clone();
        let mut call = build_call(&tool, &Value::Object(args.clone()))?;
        if let ResourceFrom::Segments { param } = &tool.resource_from {
            // Absolute-looking paths are not silently reinterpreted as relative.
            if args.get(param).and_then(Value::as_str).is_some_and(|path| {
                path.starts_with('/')
                    || path.starts_with('\\')
                    || (path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                        && path.as_bytes().get(1) == Some(&b':'))
            }) {
                return Err(invalid("environment tool path must be relative to its workspace"));
            }
            call.resource = ResourcePath::new(
                self.binding
                    .resource
                    .segments()
                    .iter()
                    .cloned()
                    .chain(call.resource.segments().iter().cloned()),
            )?;
        }
        if !call.resource.is_within(&self.binding.resource)
            || !self.scopes.iter().any(|scope| call.resource.is_within(scope))
        {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "resource is outside this environment's held tool scopes",
            ));
        }
        Ok(call)
    }
}

impl BoundTool {
    fn parameters(&self) -> Result<Value, Error> {
        let tool = &self.instances[0].tool;
        let mut params = if needs_input(tool) {
            json!({"type":"object", "properties":{"input":relocate_schema(&tool.params, "/properties/input")},
                "required":["input"], "additionalProperties":false})
        } else {
            tool.params.clone()
        };
        // One instance is no choice: the model is not asked to select it.
        if self.instances.len() == 1 {
            return Ok(params);
        }
        let schema = params.as_object_mut().ok_or_else(|| invalid("tool schema must be an object"))?;
        let envs: BTreeSet<_> = self.instances.iter().map(|instance| &instance.binding.env).collect();
        let workspaces: BTreeSet<_> =
            self.instances.iter().filter_map(|instance| instance.binding.workspace.as_ref()).collect();
        let properties = schema
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("tool properties must be an object"))?;
        properties.insert("env".to_owned(), json!({"type":"string", "enum":envs}));
        if !workspaces.is_empty() {
            properties.insert("workspace".to_owned(), json!({"type":"string", "enum":workspaces}));
        }
        let required = schema
            .entry("required")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| invalid("tool required must be an array"))?;
        required.push(json!("env"));
        if self.instances.iter().all(|instance| instance.binding.workspace.is_some()) {
            required.push(json!("workspace"));
        }
        // The enums describe available values; these branches also describe
        // the valid pairs and require workspace only for bindings that have one.
        let selectors: Vec<_> = self
            .instances
            .iter()
            .map(|instance| match &instance.binding.workspace {
                Some(workspace) => json!({"properties":{
                    "env":{"enum":[instance.binding.env]}, "workspace":{"enum":[workspace]}
                }, "required":["env","workspace"]}),
                None => json!({"properties":{"env":{"enum":[instance.binding.env]}},
                    "required":["env"], "not":{"required":["workspace"]}}),
            })
            .collect();
        schema
            .entry("allOf")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| invalid("tool allOf must be an array"))?
            .push(json!({"anyOf":selectors}));
        Ok(params)
    }

    fn resolve(&self, args: &Value) -> Result<ToolCall, Error> {
        let args = args.as_object().ok_or_else(|| invalid("environment tool arguments must be an object"))?;
        // A tool with one instance needs no selector; a given one is still checked.
        if let [only] = &self.instances[..]
            && !args.contains_key("env")
            && !args.contains_key("workspace")
        {
            return only.resolve(args.clone());
        }
        let env = args
            .get("env")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing or invalid env selector"))?;
        let workspace = match args.get("workspace") {
            None => None,
            Some(value) => Some(value.as_str().ok_or_else(|| invalid("invalid workspace selector"))?),
        };
        let instance = self
            .instances
            .iter()
            .find(|instance| {
                instance.binding.env == env && instance.binding.workspace.as_deref() == workspace
            })
            .ok_or_else(|| invalid("unknown or unavailable env/workspace selector"))?;
        instance.resolve(args.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::resolve_tool_call;
    use nucleus_protocol::{ActionName, EffectClass, Payload};

    fn path(value: &[&str]) -> ResourcePath {
        ResourcePath::new(value.iter().copied()).unwrap()
    }

    fn binding(endpoint: &str, env: &str, workspace: Option<&str>, resource: &[&str]) -> ToolBinding {
        ToolBinding {
            service: EndpointName::new("fs").unwrap(),
            endpoint: EndpointName::new(endpoint).unwrap(),
            env: env.to_owned(),
            workspace: workspace.map(str::to_owned),
            resource: path(resource),
            description: format!("Operator description for {env}"),
        }
    }

    fn tool(endpoint: &str, resource: &[&str], from: ResourceFrom) -> ToolInfo {
        let (action, params) = match &from {
            ResourceFrom::Handle => (
                "exec",
                json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
            ),
            ResourceFrom::Segments { param } => {
                ("read", json!({"type":"object","properties":{param:{"type":"string"}},"required":[param]}))
            }
            ResourceFrom::Host { param } => {
                ("fetch", json!({"type":"object","properties":{param:{"type":"string"}},"required":[param]}))
            }
        };
        ToolInfo {
            endpoint: EndpointName::new(endpoint).unwrap(),
            action: ActionName::new(action).unwrap(),
            resource: path(resource),
            effect: EffectClass::Observation,
            description: "Provider action description".to_owned(),
            endpoint_description: format!("Provider environment description for {endpoint}"),
            params,
            resource_from: from,
        }
    }

    fn file_tool(endpoint: &str, resource: &[&str]) -> ToolInfo {
        tool(endpoint, resource, ResourceFrom::Segments { param: "path".to_owned() })
    }

    fn code(catalog: &ToolCatalog, name: &str, args: Value) -> ErrorCode {
        catalog.resolve(name, &args).unwrap().unwrap_err().code
    }

    #[test]
    fn binding_json_defaults_do_not_supply_an_identity() {
        let value: ToolBinding =
            serde_json::from_value(json!({"service":"fs","endpoint":"remote-fs","env":"compute"})).unwrap();
        assert_eq!(value.resource, ResourcePath::root());
        assert_eq!(value.workspace, None);
        assert!(value.description.is_empty());
        assert!(serde_json::from_value::<ToolBinding>(json!({"service":"fs","env":"compute"})).is_err());
        assert!(
            serde_json::from_value::<ToolBinding>(
                json!({"service":"fs","endpoint":"remote-fs","env":"compute","typo":true})
            )
            .is_err()
        );
    }

    #[test]
    fn disjoint_unbound_grants_advertise_one_schema_and_keep_both_resource_paths() {
        let tools = [file_tool("memory_files", &["MEMORY.md"]), file_tool("memory_files", &["summary.md"])];
        let catalog = ToolCatalog::new(&tools, &[]).unwrap();
        assert_eq!(catalog.tools().len(), 1);
        for resource in ["MEMORY.md", "summary.md"] {
            let call = catalog.resolve("memory_files_read", &json!({"path":resource})).unwrap().unwrap();
            assert_eq!(call.resource, path(&[resource]));
        }
        let mut incompatible = tools[1].clone();
        incompatible.params = json!({"type":"object","properties":{"different":{"type":"string"}}});
        assert!(ToolCatalog::new(&[tools[0].clone(), incompatible], &[]).is_err());
    }

    #[test]
    fn unbound_endpoint_action_names_cannot_alias_another_provider() {
        let first = file_tool("a_b", &[]);
        let mut second = file_tool("a", &[]);
        second.action = ActionName::new("b_read").unwrap();
        assert!(ToolCatalog::new(&[first, second], &[]).is_err());
    }

    #[test]
    fn same_tool_routes_to_explicit_environment_and_strips_selectors() {
        let tools = [file_tool("local-fs", &[]), file_tool("remote-fs", &["chat"])];
        let bindings = [
            binding("local-fs", "local", Some("project"), &[]),
            binding("remote-fs", "compute", Some("project"), &["chat"]),
        ];
        let catalog = ToolCatalog::new(&tools, &bindings).unwrap();
        assert_eq!(catalog.tools().len(), 1);
        let schema = &catalog.tools()[0].parameters;
        assert_eq!(schema["properties"]["env"]["enum"], json!(["compute", "local"]));
        assert_eq!(schema["properties"]["workspace"]["enum"], json!(["project"]));
        assert_eq!(schema["required"], json!(["path", "env", "workspace"]));
        for (env, endpoint, expected) in [
            ("local", "local-fs", path(&["notes.txt"])),
            ("compute", "remote-fs", path(&["chat", "notes.txt"])),
        ] {
            let call = catalog
                .resolve("fs_read", &json!({"env":env,"workspace":"project","path":"notes.txt","limit":4}))
                .unwrap()
                .unwrap();
            assert_eq!(call.endpoint.as_str(), endpoint);
            assert_eq!(call.resource, expected);
            assert_eq!(call.payload, Payload::encode(&json!({"limit":4})).unwrap());
        }
        assert!(catalog.resolve("local-fs_read", &json!({"path":"notes.txt"})).is_none());
        assert!(catalog.resolve("remote-fs_read", &json!({"path":"chat/notes.txt"})).is_none());
    }

    #[test]
    fn missing_unknown_or_mismatched_selectors_never_fall_back() {
        let catalog = ToolCatalog::new(
            &[file_tool("local-fs", &[])],
            &[binding("local-fs", "local", Some("project"), &[])],
        )
        .unwrap();
        // One instance is no choice: no selector is advertised and none is needed.
        assert!(catalog.tools()[0].parameters["properties"].get("env").is_none());
        assert_eq!(catalog.resolve("fs_read", &json!({"path":"a"})).unwrap().unwrap().resource, path(&["a"]));
        // With two, a missing selector still fails rather than picking one.
        let two = ToolCatalog::new(
            &[file_tool("local-fs", &[]), file_tool("remote-fs", &[])],
            &[
                binding("local-fs", "local", Some("project"), &[]),
                binding("remote-fs", "compute", Some("project"), &[]),
            ],
        )
        .unwrap();
        assert_eq!(code(&two, "fs_read", json!({"path":"a"})), ErrorCode::InvalidInput);
        for args in [
            json!({"env":null,"workspace":"project","path":"a"}),
            json!({"env":"compute","workspace":"project","path":"a"}),
            json!({"env":"local","path":"a"}),
            json!({"env":"local","workspace":"elsewhere","path":"a"}),
            json!({"env":"local","workspace":7,"path":"a"}),
            Value::Null,
            json!([]),
        ] {
            assert_eq!(code(&catalog, "fs_read", args), ErrorCode::InvalidInput);
        }
    }

    #[test]
    fn unavailable_or_ungranted_bindings_do_not_leak_into_tools_or_context() {
        let bindings = [
            binding("local-fs", "local", None, &[]),
            binding("remote-fs", "missing-secret", None, &[]),
            binding("private-fs", "scope-secret", None, &["private"]),
        ];
        let tools = [file_tool("local-fs", &[]), file_tool("private-fs", &["public"])];
        let catalog = ToolCatalog::new(&tools, &bindings).unwrap();
        let context = catalog.context();
        assert!(!context.contains("secret"));
        assert!(!context.contains("private-fs"));
        assert!(!catalog.tools()[0].description.contains("secret"));
        assert!(catalog.tools()[0].parameters["properties"].get("env").is_none(), "one available instance");
        // While its provider is there a call reaches it; once it is gone (a
        // reconnect's snapshot) the same call fails rather than fall back to local.
        let args = json!({"env":"missing-secret","path":"a"});
        let present =
            ToolCatalog::new(&[file_tool("local-fs", &[]), file_tool("remote-fs", &[])], &bindings).unwrap();
        assert_eq!(present.resolve("fs_read", &args).unwrap().unwrap().endpoint.as_str(), "remote-fs");
        assert_eq!(code(&catalog, "fs_read", args), ErrorCode::InvalidInput);
        assert!(catalog.resolve("private-fs_read", &json!({"path":"public/a"})).is_none());
        let absent = ToolCatalog::new(&[], &bindings).unwrap();
        assert!(absent.tools().is_empty());
        assert!(absent.context().is_empty());
    }

    #[test]
    fn final_path_uses_every_held_scope_and_cannot_widen_any_of_them() {
        let catalog = ToolCatalog::new(
            &[file_tool("remote-fs", &["work", "first"]), file_tool("remote-fs", &["work", "second"])],
            &[binding("remote-fs", "compute", None, &["work"])],
        )
        .unwrap();
        assert_eq!(catalog.tools().len(), 1);
        for name in ["first/a", "second/b"] {
            assert!(catalog.resolve("fs_read", &json!({"env":"compute","path":name})).unwrap().is_ok());
        }
        for name in ["third/a", ""] {
            assert_eq!(
                code(&catalog, "fs_read", json!({"env":"compute","path":name})),
                ErrorCode::PermissionDenied
            );
        }
        for name in ["../first/a", "/first/a", "first\\a", "C:/first/a", "C:first/a", "\\\\server\\first\\a"]
        {
            assert_eq!(
                code(&catalog, "fs_read", json!({"env":"compute","path":name})),
                ErrorCode::InvalidInput
            );
        }
    }

    #[test]
    fn workspace_selection_uses_binding_resource_not_the_first_handle() {
        let tools =
            [tool("shell", &["a"], ResourceFrom::Handle), tool("shell", &["b"], ResourceFrom::Handle)];
        let bindings = [
            binding("shell", "compute", Some("first"), &["a"]),
            binding("shell", "compute", Some("second"), &["b"]),
        ];
        let catalog = ToolCatalog::new(&tools, &bindings).unwrap();
        let call = catalog
            .resolve("fs_exec", &json!({"env":"compute","workspace":"second","command":"pwd"}))
            .unwrap()
            .unwrap();
        assert_eq!(call.resource, path(&["b"]));
        assert_eq!(call.payload, Payload::encode(&json!({"command":"pwd"})).unwrap());
        let unreachable = ToolCatalog::new(&tools, &[binding("shell", "compute", None, &[])]).unwrap();
        assert!(unreachable.tools().is_empty(), "a handle-based call cannot target a broader parent scope");
    }

    #[test]
    fn host_routes_keep_url_and_enforce_binding_and_held_host_scopes() {
        let tools = [tool("web", &[], ResourceFrom::Host { param: "url".to_owned() })];
        let catalog = ToolCatalog::new(&tools, &[binding("web", "local", None, &["example.com"])]).unwrap();
        let call = catalog
            .resolve("fs_fetch", &json!({"env":"local","url":"https://Example.com/a"}))
            .unwrap()
            .unwrap();
        assert_eq!(call.resource, path(&["example.com"]));
        assert_eq!(call.payload, Payload::encode(&json!({"url":"https://Example.com/a"})).unwrap());
        assert_eq!(
            code(&catalog, "fs_fetch", json!({"env":"local","url":"https://other.com/a"})),
            ErrorCode::PermissionDenied
        );
        let narrower = ToolCatalog::new(
            &[tool("web", &["example.com"], ResourceFrom::Host { param: "url".to_owned() })],
            &[binding("web", "local", None, &[])],
        )
        .unwrap();
        assert_eq!(
            code(&narrower, "fs_fetch", json!({"env":"local","url":"https://other.com/a"})),
            ErrorCode::PermissionDenied
        );
    }

    #[test]
    fn binding_without_workspace_accepts_only_omitted_workspace() {
        let catalog = ToolCatalog::new(
            &[file_tool("local-fs", &[])],
            &[binding("local-fs", "local", None, &[]), binding("local-fs", "compute", Some("project"), &[])],
        )
        .unwrap();
        assert_eq!(catalog.tools()[0].parameters["required"], json!(["path", "env"]));
        assert!(catalog.resolve("fs_read", &json!({"env":"local","path":"a"})).unwrap().is_ok());
        assert!(
            catalog
                .resolve("fs_read", &json!({"env":"compute","workspace":"project","path":"a"}))
                .unwrap()
                .is_ok()
        );
        assert_eq!(
            code(&catalog, "fs_read", json!({"env":"local","workspace":"project","path":"a"})),
            ErrorCode::InvalidInput
        );
        assert_eq!(code(&catalog, "fs_read", json!({"env":"compute","path":"a"})), ErrorCode::InvalidInput);
    }

    #[test]
    fn duplicate_selector_is_invalid_even_if_both_endpoints_are_unavailable() {
        let first = binding("one", "local", Some("project"), &[]);
        let mut second = first.clone();
        second.endpoint = EndpointName::new("two").unwrap();
        assert!(validate_bindings(&[first.clone(), first.clone()]).is_err());
        assert!(ToolCatalog::new(&[], &[first.clone(), second]).is_err());
        for bad in ["", "   ", "local\nremote", "local\u{200B}remote"] {
            let mut bad_binding = first.clone();
            bad_binding.env = bad.to_owned();
            assert!(validate_bindings(&[bad_binding]).is_err());
        }
    }

    #[test]
    fn schema_or_resource_rule_mismatch_is_not_resolved_by_input_order() {
        let first = file_tool("one", &[]);
        let bindings = [binding("one", "local", None, &[]), binding("two", "compute", None, &[])];
        let mut second = file_tool("two", &[]);
        second.params["properties"]["path"]["maxLength"] = json!(40);
        assert!(ToolCatalog::new(&[first.clone(), second.clone()], &bindings).is_err());
        second.params = first.params.clone();
        second.resource_from = ResourceFrom::Handle;
        assert!(ToolCatalog::new(&[first, second], &bindings).is_err());
    }

    #[test]
    fn public_name_collisions_fail() {
        assert!(
            ToolCatalog::new(
                &[file_tool("fs", &[]), file_tool("remote-fs", &[])],
                &[binding("remote-fs", "compute", None, &[])]
            )
            .is_err()
        );
        let mut first_binding = binding("one", "local", None, &[]);
        first_binding.service = EndpointName::new("a_b").unwrap();
        let mut first = file_tool("one", &[]);
        first.action = ActionName::new("c").unwrap();
        let mut second_binding = binding("two", "compute", None, &[]);
        second_binding.service = EndpointName::new("a").unwrap();
        let mut second = file_tool("two", &[]);
        second.action = ActionName::new("b_c").unwrap();
        assert!(ToolCatalog::new(&[first, second], &[first_binding, second_binding]).is_err());
    }

    #[test]
    fn conflicting_provider_arguments_are_wrapped_and_round_trip_unchanged() {
        let mut provider = tool("native", &["chat"], ResourceFrom::Handle);
        provider.params = json!({"type":"object","properties":{
            "env":{"type":"object"},"workspace":{"type":"string"},"command":{"type":"string"}
        },"required":["command","env","workspace"],"additionalProperties":false});
        let catalog = ToolCatalog::new(
            &[provider.clone()],
            &[binding("native", "compute", Some("project"), &["chat"])],
        )
        .unwrap();
        let schema = &catalog.tools()[0].parameters;
        assert_eq!(schema["properties"]["input"], provider.params);
        // One instance: the outer selectors are not advertised, but still accepted.
        assert!(schema["properties"].get("env").is_none() && schema["properties"].get("workspace").is_none());
        assert_eq!(schema["required"], json!(["input"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert!(schema["properties"].get("command").is_none());
        assert!(catalog.tools()[0].description.contains("input"));
        let input =
            json!({"command":"pwd","env":{"PROFILE":"native-value"},"workspace":"provider-workspace"});
        let call = catalog
            .resolve("fs_exec", &json!({"env":"compute","workspace":"project","input":input}))
            .unwrap()
            .unwrap();
        assert_eq!(call.endpoint.as_str(), "native");
        assert_eq!(call.resource, path(&["chat"]));
        assert_eq!(call.payload, Payload::encode(&input).unwrap());
        let bare = catalog.resolve("fs_exec", &json!({"input":input})).unwrap().unwrap();
        assert_eq!(bare.payload, Payload::encode(&input).unwrap());
    }

    #[test]
    fn wrapped_local_references_keep_their_original_targets_and_payload() {
        let mut provider = tool("native", &[], ResourceFrom::Handle);
        provider.params = json!({"type":"object", "properties":{
            "env":{"$ref":"#/$defs/Options"},
            "encoded":{"$ref":"#%2f$defs%2fOptions"},
            "again":{"$ref":"#"}, "empty":{"$ref":""},
            "dynamic":{"$dynamicRef":"#/$defs/Options"},
            "anchored":{"$ref":"#option"},
            "external":{"$ref":"other.json#/$defs/Options"}
        }, "$defs":{"Options":{"$anchor":"option","type":"object","properties":{"value":{"type":"string"}}}},
            "required":["env"]});
        let catalog =
            ToolCatalog::new(&[provider.clone()], &[binding("native", "compute", None, &[])]).unwrap();
        let schema = &catalog.tools()[0].parameters;
        let input_schema = &schema["properties"]["input"];
        let reference = input_schema["properties"]["env"]["$ref"].as_str().unwrap();
        assert_eq!(reference, "#/properties/input/$defs/Options");
        assert_eq!(
            schema.pointer(reference.strip_prefix('#').unwrap()),
            Some(&provider.params["$defs"]["Options"])
        );
        assert_eq!(input_schema["properties"]["encoded"]["$ref"], "#/properties/input%2f$defs%2fOptions");
        for name in ["again", "empty"] {
            let reference = input_schema["properties"][name]["$ref"].as_str().unwrap();
            assert_eq!(schema.pointer(reference.strip_prefix('#').unwrap()), Some(input_schema));
        }
        assert_eq!(input_schema["properties"]["dynamic"]["$dynamicRef"], "#/properties/input/$defs/Options");
        assert_eq!(input_schema["properties"]["anchored"]["$ref"], "#option");
        assert_eq!(input_schema["properties"]["external"]["$ref"], "other.json#/$defs/Options");
        let input = json!({"env":{"value":"provider-value"},"again":{"env":{"value":"nested"}}});
        let call = catalog.resolve("fs_exec", &json!({"env":"compute","input":input})).unwrap().unwrap();
        assert_eq!(call.payload, Payload::encode(&input).unwrap());
    }

    #[test]
    fn wrapping_preserves_root_and_nested_schema_id_scopes() {
        let mut provider = tool("native", &[], ResourceFrom::Handle);
        provider.params = json!({"type":"object","properties":{"env":{"$ref":"#/$defs/Options"}},
            "$defs":{"Options":{"type":"string"}, "Embedded":{
                "$id":"https://example.com/embedded", "$defs":{"Options":{"type":"integer"}},
                "properties":{"own":{"$ref":"#/$defs/Options"},"self":{"$ref":"#"}}
            }, "Relative":{
                "$id":"relative.json", "$ref":"#/$defs/Options", "$defs":{"Options":{"type":"boolean"}}
            }, "Anchor":{
                "$id":"#legacy-anchor", "$ref":"#/$defs/Options"
            }, "EmptyId":{"$id":"", "$ref":"#/$defs/Options"}}});
        let catalog =
            ToolCatalog::new(&[provider.clone()], &[binding("native", "compute", None, &[])]).unwrap();
        let relocated = &catalog.tools()[0].parameters["properties"]["input"];
        for name in ["Embedded", "Relative"] {
            assert_eq!(relocated["$defs"][name], provider.params["$defs"][name]);
        }
        for name in ["Anchor", "EmptyId"] {
            assert_eq!(relocated["$defs"][name]["$ref"], "#/properties/input/$defs/Options");
        }
        for id in ["https://example.com/provider", "provider.json"] {
            provider.params["$id"] = json!(id);
            let catalog =
                ToolCatalog::new(&[provider.clone()], &[binding("native", "compute", None, &[])]).unwrap();
            assert_eq!(catalog.tools()[0].parameters["properties"]["input"], provider.params);
        }
    }

    #[test]
    fn relocation_visits_schema_keywords_without_changing_literal_data() {
        let literal = json!({"$ref":"#/$defs/Options","$id":"literal-data", "properties":{"$ref":"#"}});
        let mut provider = tool("native", &[], ResourceFrom::Handle);
        provider.params = json!({"type":"object", "properties":{"env":{"type":"string"}},
            "$defs":{"Options":{"type":"string"}},
            "default":literal, "const":literal, "enum":[literal], "examples":[literal],
            "x-annotation":literal});
        let reference = json!({"$ref":"#/$defs/Options"});
        for key in [
            "additionalProperties",
            "unevaluatedProperties",
            "not",
            "if",
            "then",
            "else",
            "contains",
            "propertyNames",
            "additionalItems",
            "unevaluatedItems",
            "contentSchema",
        ] {
            provider.params[key] = reference.clone();
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
            provider.params[key] = json!([reference]);
        }
        for key in ["definitions", "patternProperties", "dependentSchemas", "dependencies"] {
            provider.params[key] = json!({"entry":reference});
        }
        // These are property names, not a $id/$ref on the schema map itself.
        provider.params["properties"]["$id"] = reference.clone();
        provider.params["properties"]["$ref"] = reference;
        let catalog =
            ToolCatalog::new(&[provider.clone()], &[binding("native", "compute", None, &[])]).unwrap();
        let relocated = &catalog.tools()[0].parameters["properties"]["input"];
        for key in ["default", "const", "enum", "examples", "x-annotation"] {
            assert_eq!(relocated[key], provider.params[key]);
        }
        for key in [
            "additionalProperties",
            "unevaluatedProperties",
            "not",
            "if",
            "then",
            "else",
            "contains",
            "propertyNames",
            "additionalItems",
            "unevaluatedItems",
            "contentSchema",
        ] {
            assert_eq!(relocated[key]["$ref"], "#/properties/input/$defs/Options");
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
            assert_eq!(relocated[key][0]["$ref"], "#/properties/input/$defs/Options");
        }
        for key in ["definitions", "patternProperties", "dependentSchemas", "dependencies"] {
            assert_eq!(relocated[key]["entry"]["$ref"], "#/properties/input/$defs/Options");
        }
        for key in ["$id", "$ref"] {
            assert_eq!(relocated["properties"][key]["$ref"], "#/properties/input/$defs/Options");
        }
    }

    #[test]
    fn recursive_schema_without_selector_conflicts_keeps_selectors_out_of_child_nodes() {
        let mut provider = tool("recursive", &[], ResourceFrom::Handle);
        provider.params = json!({"type":"object","properties":{
            "name":{"type":"string"},"children":{"type":"array","items":{"$ref":"#"}}
        },"required":["name"],"additionalProperties":false});
        for identified in [false, true] {
            if identified {
                provider.params["$id"] = json!("https://example.com/tree");
            }
            let catalog =
                ToolCatalog::new(&[provider.clone()], &[binding("recursive", "compute", None, &[])]).unwrap();
            let schema = &catalog.tools()[0].parameters;
            let input_schema = &schema["properties"]["input"];
            assert_eq!(input_schema["required"], json!(["name"]));
            assert!(input_schema["properties"].get("env").is_none());
            assert_eq!(
                input_schema["properties"]["children"]["items"]["$ref"],
                if identified { "#" } else { "#/properties/input" }
            );
            let input = json!({"name":"parent","children":[{"name":"child","children":[]}]});
            let call = catalog.resolve("fs_exec", &json!({"env":"compute","input":input})).unwrap().unwrap();
            assert_eq!(call.payload, Payload::encode(&input).unwrap());
        }
    }

    #[test]
    fn root_combinators_and_property_constraints_are_preserved_inside_input() {
        let mut provider = tool("composed", &[], ResourceFrom::Handle);
        for params in [
            json!({"type":"object","allOf":[{
                "type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false
            }],"propertyNames":{"pattern":"^name$"},"minProperties":1,"maxProperties":1}),
            json!({"type":"object","const":{"name":"provider"}}),
            json!({"type":"object","enum":[{"name":"provider"}]}),
        ] {
            provider.params = params;
            let catalog =
                ToolCatalog::new(&[provider.clone()], &[binding("composed", "compute", None, &[])]).unwrap();
            assert_eq!(catalog.tools()[0].parameters["properties"]["input"], provider.params);
            let input = json!({"name":"provider"});
            let call = catalog.resolve("fs_exec", &json!({"env":"compute","input":input})).unwrap().unwrap();
            assert_eq!(call.payload, Payload::encode(&input).unwrap());
        }
    }

    #[test]
    fn literal_reference_annotations_do_not_wrap_a_simple_tool() {
        let mut provider = file_tool("fs", &[]);
        for key in ["default", "examples", "x-annotation"] {
            provider.params[key] = json!({"$ref":"#"});
        }
        let catalog = ToolCatalog::new(&[provider], &[binding("fs", "local", None, &[])]).unwrap();
        assert!(catalog.tools()[0].parameters["properties"].get("input").is_none());
        assert!(catalog.resolve("fs_read", &json!({"env":"local","path":"file.txt"})).unwrap().is_ok());
    }

    #[test]
    fn wrapped_calls_require_object_input_and_reject_mixed_flat_arguments() {
        for reserved in ["env", "workspace"] {
            let mut provider = tool("native", &[], ResourceFrom::Handle);
            provider.params["properties"][reserved] = json!({"type":"string"});
            let catalog = ToolCatalog::new(&[provider], &[binding("native", "compute", None, &[])]).unwrap();
            for args in [
                json!({"env":"compute","command":"pwd"}),
                json!({"env":"compute","input":null}),
                json!({"env":"compute","input":[]}),
                json!({"env":"compute","input":"pwd"}),
                json!({"env":"compute","input":{"command":"pwd"},"command":"ls"}),
                json!({"env":"compute","input":{"command":"pwd"},"extra":true}),
                json!({"env":"missing","input":{"env":"compute","command":"pwd"}}),
            ] {
                assert_eq!(code(&catalog, "fs_exec", args), ErrorCode::InvalidInput);
            }
            let input = json!({"command":"pwd",reserved:"original"});
            let call = catalog.resolve("fs_exec", &json!({"env":"compute","input":input})).unwrap().unwrap();
            assert_eq!(call.payload, Payload::encode(&input).unwrap());
        }
    }

    #[test]
    fn wrapped_resource_parameters_are_resolved_from_input() {
        let file = tool("native-fs", &["chat"], ResourceFrom::Segments { param: "env".to_owned() });
        let catalog = ToolCatalog::new(&[file], &[binding("native-fs", "compute", None, &["chat"])]).unwrap();
        let call = catalog
            .resolve("fs_read", &json!({"env":"compute","input":{"env":"file.txt","limit":4}}))
            .unwrap()
            .unwrap();
        assert_eq!(call.resource, path(&["chat", "file.txt"]));
        assert_eq!(call.payload, Payload::encode(&json!({"limit":4})).unwrap());
        let web = tool("native-web", &["example.com"], ResourceFrom::Host { param: "workspace".to_owned() });
        let catalog =
            ToolCatalog::new(&[web], &[binding("native-web", "compute", Some("project"), &[])]).unwrap();
        let input = json!({"workspace":"https://Example.com/a"});
        let call = catalog
            .resolve("fs_fetch", &json!({"env":"compute","workspace":"project","input":input}))
            .unwrap()
            .unwrap();
        assert_eq!(call.resource, path(&["example.com"]));
        assert_eq!(call.payload, Payload::encode(&input).unwrap());
        assert_eq!(
            code(
                &catalog,
                "fs_fetch",
                json!({"env":"compute","workspace":"project","input":{"workspace":"https://other.com/a"}})
            ),
            ErrorCode::PermissionDenied
        );
    }

    #[test]
    fn unbound_resolution_keeps_paths_and_provider_descriptions() {
        let tools = [file_tool("fs", &["one"]), file_tool("fs", &["two"])];
        let catalog = ToolCatalog::new(&tools, &[]).unwrap();
        // Presentation merges the duplicate schema; resolution accepts each
        // tool's paths and rejects invalid arguments.
        assert_eq!(catalog.tools().len(), 1);
        assert_eq!(catalog.tools()[0].parameters, tools[0].params);
        for args in [json!({"path":"two/file"}), json!({"path":"../bad"}), Value::Null] {
            assert_eq!(catalog.resolve("fs_read", &args), resolve_tool_call(&tools, "fs_read", &args));
        }
        assert!(catalog.context().contains(&tools[0].endpoint_description));
        assert!(catalog.tools()[0].description.contains(&tools[0].description));
        let bound = ToolCatalog::new(&tools, &[binding("fs", "local", None, &[])]).unwrap();
        assert!(bound.tools()[0].description.contains(&tools[0].description));
        assert!(bound.context().contains(&tools[0].endpoint_description));
        assert!(bound.context().contains("Operator description for local"));
    }

    #[test]
    fn mixed_bindings_cannot_send_remote_selectors_to_a_local_guard_action() {
        let mut local_remove = file_tool("fs", &[]);
        local_remove.action = ActionName::new("remove").unwrap();
        let catalog = ToolCatalog::new(
            &[local_remove, file_tool("compute-fs", &["chat"])],
            &[binding("compute-fs", "compute", Some("project"), &["chat"])],
        )
        .unwrap();
        for args in [
            json!({"env":"compute","workspace":"project","path":"same-name.txt"}),
            json!({"env":"compute","path":"same-name.txt"}),
            json!({"workspace":"project","path":"same-name.txt"}),
        ] {
            assert_eq!(code(&catalog, "fs_remove", args), ErrorCode::InvalidInput);
        }
        let local = catalog.resolve("fs_remove", &json!({"path":"same-name.txt"})).unwrap().unwrap();
        assert_eq!(local.endpoint.as_str(), "fs");
        let remote = catalog
            .resolve("fs_read", &json!({"env":"compute","workspace":"project","path":"same-name.txt"}))
            .unwrap()
            .unwrap();
        assert_eq!(remote.endpoint.as_str(), "compute-fs");
    }

    #[test]
    fn unbound_tools_reject_undeclared_selectors_with_or_without_other_bindings() {
        let tools = [file_tool("fs", &[]), file_tool("compute-fs", &[])];
        let mut remote = binding("compute-fs", "compute", None, &[]);
        remote.service = EndpointName::new("remote_files").unwrap();
        for bindings in [vec![], vec![remote]] {
            let catalog = ToolCatalog::new(&tools, &bindings).unwrap();
            for selector in [json!({"env":"compute"}), json!({"workspace":"project"}), json!({"env":null})] {
                let mut args = selector;
                args["path"] = json!("file.txt");
                assert_eq!(code(&catalog, "fs_read", args), ErrorCode::InvalidInput);
            }
            assert!(catalog.resolve("fs_read", &json!({"path":"file.txt"})).unwrap().is_ok());
        }
    }

    #[test]
    fn unbound_provider_native_env_and_workspace_arguments_keep_their_meaning() {
        for native in ["env", "workspace"] {
            let mut provider = file_tool("native", &[]);
            provider.params["properties"][native] = json!({"type":"object"});
            let catalog = ToolCatalog::new(&[provider], &[]).unwrap();
            let args = json!({"path":"file.txt",native:{"key":"provider-owned-value"}});
            let call = catalog.resolve("native_read", &args).unwrap().unwrap();
            assert_eq!(
                call.payload,
                Payload::encode(&json!({native:{"key":"provider-owned-value"}})).unwrap()
            );
        }
    }

    #[test]
    fn full_environment_descriptions_appear_once_across_actions_and_scopes() {
        let mut tools = vec![file_tool("fs", &["one"]), file_tool("fs", &["two"])];
        let mut write = tools[0].clone();
        write.action = ActionName::new("write").unwrap();
        tools.push(write);
        let bindings = [binding("fs", "local", None, &[])];
        for catalog in [ToolCatalog::new(&tools, &[]).unwrap(), ToolCatalog::new(&tools, &bindings).unwrap()]
        {
            let context = catalog.context();
            assert_eq!(context.matches(&tools[0].endpoint_description).count(), 1);
            for tool in catalog.tools() {
                assert!(!tool.description.contains(&tools[0].endpoint_description));
                assert!(tool.description.contains(&tools[0].description));
            }
        }
    }
}
