# Developing Open Math OS plugins

For HTTP MCP use `mcp_connect(name,url,tools)` directly. This guide is for developers.

## Environment and ownership

plugin_capabilities reports execution_context, IPC version and held grants. Use its
paths. All env values run in the user's VM. Matching env/workspace file/shell
tools share files. A child stays in its parent's environment; others use IPC.
plugin_list lists only this chat's instances, not boot services or system inventory.

Identify a program's owner and source before editing; plugin_replace manages only
your instances. Keep user code outside the read-only runtime. Packages grant nothing.
execution_context.sandboxed says what a child is. false (a project in the VM): it runs
as your project's Linux user, like your shell commands: the same files at the same
paths, the VM's network. Its request has NO binds and NO network field (either is
refused); use real paths. true or absent: it sees exactly its binds.
fs_create makes no parent directories; create them first.

Grants/binds/network cannot exceed the parent. Each grant object delegates from one
held handle; if plugin_capabilities shows per-action handles, use one grant per action. Nonempty actions create an endpoint
and substitute {endpoint}; call plugin-NAME_ACTION with env. Empty/omitted actions
create no endpoint: startup means alive, not ready. Omitted grants give no service access.

## Native IPC, network and MCP

Native providers use the reported IPC version. stdout is
4-byte big-endian JSON byte length then UTF-8 {v:VERSION,msg:MESSAGE}; read exact
lengths, log to stderr. hello provides identity/local handles. Find kernel provide
authority on ["endpoint",allocated_name], then publish the endpoint's interface:
{provide:{request:1,authority:HANDLE,interface:{endpoint:NAME,version:1,description:TEXT,
actions:{read:{effect:"observation",tool:{description:TEXT,params:JSON_SCHEMA,
resource_from:"handle"}}}}}}.

Await response.request=1/result.Ok.bound; Err fails startup.
Dispatch includes dispatch, caller, action, resource, payload, remaining_ms. Validate
and reply with ORIGINAL dispatch ID: {reply:{dispatch:42,result:{Ok:JSON}}}.
Errors: {Err:{code:invalid_input|permission_denied|unknown_outcome|service_error,message:TEXT}}.
Never fabricate identity. Use held handles for {call:{request:3,handle:18,action:NAME,
resource:PATH,payload:JSON,deadline_ms:10000}}; match response.request and unwrap
result.Ok.payload. Tool-routing env/workspace are not native service payloads.
web resources must cover URL host. Bound sizes/concurrency/deadlines. On
cancel_dispatch send downstream {cancel:{request:3}}; ignore late results. This
does not undo effects. Request IDs and dispatch IDs are separate.

A sandboxed child: network=true in execution_context permits requesting
program.network=true, not proof of connectivity; false forbids it, omitted is false.
A web grant authorizes IPC, not sockets. Never disable TLS verification. Install
dependencies with the actual interpreter.

For MCP use execution_context.mcp_executable with args:
["--endpoint","{endpoint}","--command",PYTHON (python_executable),"--arg","WORKSPACE/server.py" (workspace_dir),
"--tool","forecast=forecast"], actions=["forecast"]. Sandboxed: bind interpreter/source/deps.
Adapter uses Nucleus frames; MCP server uses newline JSON-RPC initialization/list/call. MCP children have no Nucleus reverse bridge; use a native
provider for service IPC.

For remote MCP replace --command/--arg with --url https://SERVER/mcp. No OAuth.

## Replace and stop

plugin_stop confirms owned subtree cleanup. Edit authorized files, build if needed,
then plugin_start the same name with fresh explicit grants. plugin_replace stops the old
instance, confirms cleanup, then starts the new one; failure leaves the name stopped.
File edits alone do not restart.

To exit with children send {exit:{request:4}}, await result.Ok="done" (descendants
cleaned up), then exit. On msg="terminate" clean up and exit; do not request Exit.

Source persists; instances die with their creating chat process and are not
reinstalled on reconnect. plugin_list reports unfinished cleanup/pending:<pid>; never
delete journals to hide failures. Test results, denials and cleanup.
