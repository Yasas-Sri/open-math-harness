# Open Math Harness

Nucleus, the runtime of Open Math OS, is an application-level, capability-based
microkernel for AI agents. It runs on an existing operating system, in Open Math OS each
team's Linux VM; it does not replace the host kernel. Agent loops, model providers, tools,
MCP bridges, memory and orchestration are replaceable services. The kernel supplies the
mechanisms that keep their authority, communication and lifetimes explicit. Keep existing
executable names, protocol identifiers and data locations: VMs, the platform and the apps
depend on them. Open Math OS keeps this repository in its `harness/` folder and builds and
ships `deploy/vm/` as the VM runtime.

These are development requirements, not a claim that every mechanism already holds
everywhere. Distinguish what is designed, implemented and tested when you report.

## Rules

- Never commit secrets, credentials, real user transcripts or private machine details.
- Do not bypass a failing check, sandbox or approval gate; report it.
- Work on a branch and open a pull request by default. Pushing or merging into `main`,
  rewriting shared history and force-pushing require explicit user confirmation.
  An explicit user request authorizes the specified action; do not ask again when
  that authorization is already clear.
- A plan still under discussion is not a request to build it.
- Documentation lives in comments at the code it describes. Do not add Markdown
  documents (`tools/check_markdown.py` lists the exceptions). A change that makes a
  comment wrong corrects it in the same change.

## Before changing code

Read the README, applicable nested `AGENTS.md` files and the module docs of the code you
change and of what it calls. Inspect manifests, lockfiles, CI and nearby tests before
choosing commands or dependencies. Preserve unrelated work in the working tree. The
workspace is Rust and Cargo, pinned by `rust-toolchain.toml`; the README lists the commands
CI runs. Do not invent commands, directories, supported platforms or compatibility
promises.

When a change alters authority, isolation, lifecycle, IPC or persistence, state its
invariants and failure behavior in the module docs of the code it changes, in the same
change.

## Architecture: mechanisms in the kernel, behavior in services

The kernel owns only mechanisms that must be enforced the same way for every service:

- Authenticated process identities, capability checks, attenuation and revocation.
- Handle ownership, namespaces, service endpoint identities and generations.
- IPC admission and dispatch, bounded queues, cancellation and resource accounting.
- Process and resource lifetime relationships, and security-relevant events.

Agent loops, prompts, planning, model routing, memory, tools, MCP adapters, restart
policies, approval interfaces, session storage and scheduling or quota policy stay outside
the kernel. Policy belongs outside; its enforcement inside.

Privilege lives in the kernel. A trusted service, such as the launcher, a supervisor or a
guard that holds a dangerous capability on others' behalf, is still part of what must be
trusted: moving code out of the kernel does not make it safe against hostile input. Its
module docs say what it can do and what its compromise means. Ordinary programs cannot
replace enforcement, grant themselves authority or turn a denial into an allow.

Before adding a kernel feature, explain why it cannot be built through existing service
interfaces without weakening an invariant. Convenience for one agent, provider or
deployment is not a reason.

The kernel must not import an agent, an LLM SDK, an MCP adapter, a UI or a domain
workflow. Programs depend on the public protocol, interfaces and SDK, never on private
kernel state. `tests/tests/architecture.rs` enforces the direction: each crate may use only
the crates its rule names, no program links the kernel or the platform, and the kernel and
the launcher link no program.

## Organization

```text
kernel/            identity, capabilities, handles, IPC, lifetime enforcement
protocol/          versioned messages, errors, identifiers
interfaces/        each endpoint's interface, shared by its provider and its consumers
program-files/     the programs: services, agents and their resources, one binary each
sdk/               clients of the public kernel and service protocols
cli/               the launcher: boot, configuration and operator interaction
platform/          host processes, IPC, confinement and Linux identities
prompts/           the agents' prompts and their assembly
memories/          the memory pipeline's reading and writing
deploy/vm/         the composition a VM runs, and its installation
examples/slice/    a composition that runs without a platform or a model
tests/             cross-service, security, recovery and integration tests
tools/             packaging and repository checks
third-party/       the notices of material adapted from other projects
```

Keep unit tests beside their owners. Do not create empty packages, and do not reorganize
without a concrete need. A program's authority comes only from the composition that
starts it, never from its own files.

## Core vocabulary

A **process** is a runtime principal with an owner, state, resource limits and
capabilities. A **session** is durable interaction history; it is not a live process or an
authorization grant. A **service** exposes a versioned interface through an endpoint. A
**handle** identifies an authorized reference to an endpoint. A **capability** grants
particular actions on particular resources under explicit constraints. A service name, a
tool description or a discoverable endpoint grants no authority.

## Capabilities and isolation

- Deny by default. Know a caller by the channel the kernel reads it from, never by a
  PID, role or field it sends, and look its handles up under that identity.
- Model output, tool results, repository content, skills, what the platform or a VM
  returns, and the output of any process that runs untrusted code (Lean checking a
  candidate proof, for example) are untrusted input. Text cannot mint a capability,
  approve an operation, change the trust configuration or decide a verification result.
- Delegation keeps or narrows actions, resource scope, expiry and delegability, and a
  shared budget does not multiply when it is delegated. No operation moves a capability
  between running processes: authority reaches a child only in the grants of its spawn or
  with a fresh endpoint.
- Authority is checked at dispatch, queued requests included. Revocation refuses later
  admissions and follows the delegation chain; cancelling work already dispatched is a
  separate matter.
- A handle belongs to its owner's table and to the endpoint generation it was bound to.
  Fabricated, foreign, expired, revoked and stale handles are refused, and a stale one is
  only ever rebound explicitly. Opaque-looking strings are not a security mechanism.
- Credentials stay in narrowly privileged brokers or providers. They never reach models,
  transcripts, plugin configuration, inherited child environments or logs.
- Confinement fails closed. A program of the runtime's own identity runs in its sandbox
  (bubblewrap) or not at all. A project's process is, by design, a plain process of the
  `projects` Linux user without a sandbox: it starts only as that user, in its project's
  cgroup and, when its run says so, without network, or it does not start. Projects are not
  isolated from one another; the Unix boundary between them and the runtime is what
  protects the runtime, so keep it whole.
- Programs an agent makes are ordinary child processes whose grants are checked against
  the agent's own authority. Code inside a process is trusted by that process; nothing in
  one process is a sandbox for hostile code.
- Check what is actually opened, not the name that was asked for: filesystem scope on the
  opened descriptor (no-follow resolution, no check-then-use), network scope on every
  redirect and resolved address, a verification result in a process the candidate cannot
  write to.
- A shell command has explicit arguments, working directory, environment, inherited
  descriptors, cancellation and descendants.
- An approval is bound to the principal, the target, the exact operation parameters and
  the policy version, is used once, and is checked before the protected action is
  dispatched.

Capability confinement limits the actions available; it does not make permitted actions
harmless or prevent misuse of data a program may legitimately read.

## Lifetimes and composition

Every registration and resource acquisition has an owner and a release path. Never change
a global tool list, prompt fragment, listener or registry without ownership tracking.

If activation fails halfway, release what was already acquired. Cleanup is safe to repeat,
bounded by a deadline and observable when it fails; after one release fails, the
independent ones still run, and incomplete cleanup is never reported as success. Release
dependents before dependencies. A child belongs to an explicit supervisor and cannot
outlive its owner by accident.

Shutdown stops new work, ends further authority, cancels owned work and releases in a known
order. Cleanup that needs a supervisor's authority never restores the terminated program's
privileges.

The composition is the launcher's validated configuration: it boots each program after the
providers of the endpoints it is granted, and missing providers, bad grants and invalid
configuration are explicit errors. An agent may start a program of its own, but only with
grants its own authority covers.

Replacing a provider gives it a new generation. Never retarget an old handle to a new
provider silently, or replay a side-effecting call because a dependency came back.

## Effects and failure

Each interface declares how its actions behave, by what they do rather than by their
names:

| Effect class | What it means |
| --- | --- |
| Observation | Does not intentionally change the target; still counts for data exposure, cost and nondeterminism. |
| Reversible mutation | Has an inverse for a specified owned state, under stated interference assumptions. |
| Compensatable | Has a separate authorized action that mitigates its consequences without restoring history. |
| Irreversible | Has no inverse: policy and approval are enforced before dispatch. |

An interrupted call that is not an observation has an unknown outcome. Closing a connection
releases a resource; it does not reverse requests sent through it. Deleting a temporary
file does not undo the disclosure of its contents, and stopping an agent does not unsend
what it sent. The kernel enforces authority and ownership; it is not a transaction
coordinator.

The default failure policy is fail and start clean: when a process, a connection or the
runtime is lost, report it, release what it held and start fresh; long work continues
from what it recorded. Do not build in-place recovery, reconnection that restores live
handles, or replay of side-effecting calls.

Where an operation with external effects must survive a crash, record its intent and a
stable operation ID durably before dispatch, and the outcome after. A crash or timeout in
between is an unknown outcome, not a failure. Retry an uncertain side effect only through
idempotency or an authoritative status query; never promise exactly-once. Do not claim
rollback, compensation, automatic resume or crash recovery an operation does not have.

Fix the cause: remove the step that causes a problem instead of adding retries, buffers,
caps or checks around it, and fix it fully in one change rather than in phases. When the
same area needs a third round of fixes, stop and propose a smaller requirement.

## IPC and service interfaces

Each capability has an interface (`interfaces/`), a provider and its consumers; keep the
roles explicit even when they share a crate. The SDK takes the same authorization path as
any client; there are no privileged shortcuts.

Version wire formats. Validate input where it enters: model and tool JSON, configuration,
IPC, files and host processes, and refuse unknown fields in configuration and requests. Do
not validate again inside trusted typed code without a new reason to distrust the value.

Each request has its identity, deadline and cancellation; payloads and queues have limits,
and a slow consumer never causes unbounded buffering. Keep invalid input, permission
denial, stale handle, unavailable service, cancellation, timeout and unknown outcome apart.
Hold no global lock while waiting on a model, the network, a provider or a cleanup. Bound
retries and concurrency, keep one service's failure from unrelated work, and let discovery
and subscriptions leak nothing across namespaces.

## Sessions and persistence

A session's log is what its agent continues from: record what the model was given so it
can be rebuilt. Credential headers are not model input and never enter it. Rebuilding a
session runs nothing again and carries no authority; resuming is a new authorized
execution, and old handles or credentials are never read back as current authority.

Keep security events, approval records and recovery records apart from the user-visible
conversation, and decide their access, retention and redaction deliberately.

Durable formats are versioned separately from interfaces. Messages between the platform
and a VM, and the files a VM keeps, are read by runtimes and platforms of other versions:
change them only in ways those still accept, or migrate them explicitly. Refuse formats
newer than you understand instead of reinterpreting them. Publish atomically where that is
promised, keep the source data while migrating, and never rewrite an old fixture to hide a
broken migration.

## Development and verification

Use the declared toolchain and the commands CI runs. Before running tests, read the code
against these requirements and reason it through. Run the smallest meaningful set of
checks for the changed surface, and broaden it for cross-cutting changes; a full suite does
not replace a test of the changed failure mode.

Default tests need no paid API or production credential: they use the fixture agent and
deterministic providers, and the kernel has no clock of its own. Tests against real
providers or backends are explicit opt-in, ignored with their reason. A mocked
authorization test does not establish isolation: keep tests with real child processes and
real bubblewrap, and one against real Lean.

Test a behavior once, through its public interface, end to end where it matters; do not
repeat it at another layer. A bug gets at most one regression test, and it fails before
the fix. The risks worth a test:

- Forged or foreign handles, widened delegation, revoked queued calls, stale generations,
  approval and request mismatch.
- Partial activation, cancellation during acquisition, failing cleanup, provider
  replacement, descendant termination.
- Slow consumers, full queues, oversized messages, disconnects, service crashes that must
  not fail unrelated work.
- Crashes around external dispatch, unknown outcomes, idempotent retry, rebuilding a
  session without running anything.
- Filesystem escapes, restricted network, environment leaks, refusal to start without the
  required confinement.

Test observable behavior, not private structure or log text. Never weaken assertions,
normalize away a regression or widen privileges to make a test pass. A test that cannot run
says so instead of passing. Enforcement runs in CI; a comment describing an invariant is not
its enforcement.

## Implementation conventions

Prefer small, explicit modules and maintained dependencies that remove more complexity
than they add. Use strict types, typed identifiers, structured errors and explicit state
machines; unsafe code is forbidden except in `platform/preexec`, the isolated Linux
pre-exec boundary with its own lints. Keep its hooks limited to async-signal-safe
system calls; failed setup must fail the spawn. See its module docs and Cargo manifest.
Avoid deep imports, circular dependencies, hidden
fallbacks, speculative abstraction layers, settings nobody sets and provider-specific
branches in generic code. Every fact has one source; other places take it from there
instead of keeping their own copy.

Deployment-varying limits are validated configuration; protocol constants and security
invariants are not settings. Misconfiguration fails at startup or at the earliest point it
can be known, and never silently drops a required protection or service.

An interface change updates its consumers, tests and comments in the same change; within
one runtime interfaces may change deliberately, and a compatibility shim needs a real
consumer. Report the exact checks run, their results, what did not run and what remains
unverified. Do not describe a prototype as production-ready or formally verified.

## Vertical slices

Nucleus started as one working vertical slice instead of a framework of empty abstractions:
a launcher, a kernel with authenticated IPC and scoped handles, a keyless fixture agent and
a confined filesystem service. `examples/slice` is that slice: a worker receives narrower
authority and forbidden actions are refused before they run. The model adapter and the MCP
bridge came through the same public interfaces, not special kernel paths; build new
capabilities the same way, the smallest path that works end to end.

Do not add distributed consensus, a plugin marketplace, SDKs in other languages, a custom
transport stack or elaborate UI without a concrete need. Self-modification takes the same
authorized path as an operator's change: it is never permission to rewrite the running
kernel or its own security policy.

## Maintaining these instructions

Keep this file to durable development requirements; what the code does now belongs in its
module docs. `AGENTS.md` is the canonical instruction file and `CLAUDE.md` includes it.
Remove a rule when the implementation makes it false.
