# Prime Agent

Prime Agent runs durable agent conversations and lets optional plugins customize inference, execution, context, and memory. This language distinguishes the recorded conversation from the context presented to a model.

## Conversations and execution

**Session**:
A durable conversation identity with its own history and execution state. Resuming a session continues that identity; starting a fresh session creates another.
_Avoid_: Chat window, worker

**Canonical history**:
The durable record of a session's events, messages, and branch relationships. A model-facing view or compressed representation is not the canonical history.
_Avoid_: Context window, memory view

**Session branch**:
A path through a session's recorded history ending at a selected entry. Different branches can share earlier entries without sharing their later context.
_Avoid_: Separate session, transcript copy

**Turn**:
An agent run initiated by accepted input or an explicit continuation and ending when the agent completes or is interrupted. A turn can contain several model calls and tool executions.
_Avoid_: Model call, message

**Model call**:
One inference request and its response within a turn. Auxiliary tasks such as summarization can make model calls without becoming separate user conversations.
_Avoid_: Turn, session

**Session worker**:
The execution owner of an active session. A client can detach without ending that session's work.
_Avoid_: Client, terminal

**Supervisor**:
The owner of active-worker discovery, routing, and recovery. It is distinct from the worker that executes a session.
_Avoid_: Session engine, client

**Client**:
An interface through which a person or application observes and controls sessions. A client does not define a session's durable identity.
_Avoid_: Session, worker

## Plugins and contributions

**Plugin**:
A named, optional source of agent behavior and contributions with a defined activation lifecycle. A plugin's identity is independent of the implementation language used to provide it.
_Avoid_: Skill, MCP server, resource package

**Plugin host**:
The owner of plugin activation, invocation, contribution registration, and retirement. It supplies capabilities to plugins without making their individual policies part of the host itself.
_Avoid_: Model kernel, plugin

**Plugin instance**:
An activated plugin's live state for a particular execution scope. Separate instances can share the same plugin identity without sharing mutable state.
_Avoid_: Plugin source, global singleton

**Plugin generation**:
One version of a plugin instance's registrations and live state between activation and retirement. A replacement generation does not make an earlier invocation current again.
_Avoid_: Package version, session generation

**Plugin contribution**:
A named capability an active plugin supplies to the host, such as a command, hook, provider, or context strategy. Contributions belong to the plugin activation that registered them.
_Avoid_: Patch, permanent core feature

**Plugin export**:
A function or value a plugin deliberately makes available to callers. Internal plugin functions are not exports merely because they exist.
_Avoid_: Every plugin function, tool

**Plugin command**:
A user-invoked command contributed by a plugin. It is distinct from a model-facing tool and from a prompt template.
_Avoid_: Tool, prompt template

**Activation**:
The interval during which a plugin's instance and contributions are available. A disabled plugin has no active contributions, although its saved data can remain.
_Avoid_: Installation, import

**Retirement**:
The end of a plugin generation's authority to contribute behavior or publish invocation results. Retirement is distinct from deleting the plugin's saved data.
_Avoid_: Data deletion, session shutdown

**Skill**:
Discoverable instructions or callable functionality made available to an agent. A skill is not, by itself, a host lifecycle plugin.
_Avoid_: Plugin, mandatory policy

**Resource package**:
A collection used to distribute discoverable agent resources. Distributing a resource does not itself activate a plugin or define an execution protocol.
_Avoid_: Plugin host, extension runtime

## Hooks and invocation

**Hook stage**:
A defined point in agent execution at which registered hooks receive a stage-specific input. The stage defines which observations and decisions are meaningful.
_Avoid_: Arbitrary callback, every internal step

**Control hook**:
A hook whose supported decision can affect the operation at its stage. Its permitted effects are part of that stage's contract.
_Avoid_: Observer, notification

**Observer hook**:
A hook that receives execution information without deciding or rewriting the observed operation. Observation does not imply guaranteed audit delivery.
_Avoid_: Control hook, veto

**Hook outcome**:
The stage-specific result of a hook invocation, including any supported transformation or decision. A hook failure is distinct from an intentional decision.
_Avoid_: Arbitrary mutation, callback error

**Context contribution**:
Material explicitly supplied for inclusion in a model-facing context. A contribution is not automatically a new entry in canonical history.
_Avoid_: Recorded message, base prompt

## Inference and routing

**Inference provider**:
A named source of model inference with associated model capabilities and authentication. Several inference providers can use the same request protocol.
_Avoid_: Model, transport adapter, MCP server

**Transport adapter**:
The translator between the agent's inference representation and a provider's request and streaming-response protocol.
_Avoid_: Provider identity, inference gateway

**Provider registration**:
The association of a provider identity with its models, capabilities, authentication, and transport behavior within a defined scope.
_Avoid_: Global provider mutation, model list

**Model definition**:
The identity and advertised capabilities of one model offered by a provider. It is distinct from the parameters chosen for a particular request.
_Avoid_: Request configuration, provider

**Model catalog**:
The available model definitions and their provenance for a defined discovery scope.
_Avoid_: Selected model, provider registration

**Catalog refresh**:
An operation that updates discovered model information and reports its outcome. Refreshing a catalog is distinct from changing a session's selected model.
_Avoid_: Model switch, login

**Inference gateway**:
An intermediary that routes inference requests to upstream deployments. Using a gateway is distinct from registering a direct provider.
_Avoid_: MCP gateway, plugin host

**MCP gateway**:
An intermediary that presents and routes tool capabilities from upstream MCP servers. It is not an inference provider merely because it shares an endpoint or operator with one.
_Avoid_: Inference gateway, provider adapter

**Tool integration**:
A callable operation made available to the agent through a defined tool interface. An external tool integration is not a general way to customize the host's inference or lifecycle.
_Avoid_: Plugin system, provider registration

**Client protocol**:
A contract through which an external client communicates with the agent host. Client access does not by itself confer plugin registration or context-strategy authority.
_Avoid_: Plugin interface, tool protocol

## Context and memory

**Context view**:
The ordered material presented for a particular model call, derived from history and explicit contributions. It can differ from the complete recorded conversation.
_Avoid_: Canonical history, session file

**Context strategy**:
The selected policy that assembles and maintains a session's model-facing context view. It can use compaction, retrieval, or another representation of history.
_Avoid_: Summary prompt, canonical history store

**Compaction strategy**:
A method for producing a smaller representation of selected history. A compaction strategy is narrower than the policy controlling the entire context view.
_Avoid_: Context strategy, history deletion

**Compaction proposal**:
A candidate compacted representation together with the history it covers and the continuation it retains. A proposal is distinct from its acceptance into durable session state.
_Avoid_: Committed compaction, summary string

**Compaction chain**:
An ordered selection of compaction strategies, tried until one produces an acceptable proposal or the operation ends. It is distinct from the session's context strategy, which decides whether compaction is appropriate at all.
_Avoid_: Context-strategy chain, hook order

**Compaction attempt**:
One compaction strategy's work on a fixed selection of history. Declining the attempt is distinct from producing an acceptable proposal, failing, or being cancelled.
_Avoid_: Committed compaction, full fallback chain

**Compaction fallback**:
Advancing to the next configured compaction strategy when an earlier attempt cannot provide an acceptable result. It is distinct from retrying the same attempt or replacing the active context strategy.
_Avoid_: Retry, model switch, context-strategy switch

**Server-side compaction**:
Compaction performed by an inference provider rather than by a local archive renderer or a host-managed summarization operation. Its result can contain provider-native context rather than a portable text summary.
_Avoid_: Standard summarization, local bitmap archive

**Provider-native context**:
Context material whose meaning or validity depends on a provider's protocol or model lineage. It is distinct from portable conversation text and images.
_Avoid_: Universal summary, plain text archive

**Compaction archive**:
The persisted representation of compacted history and the information needed to reconstruct its contribution to context. An archive can include summary text, context assets, or provider-native context.
_Avoid_: Summary, canonical history

**Context asset**:
A non-text resource referenced by a context representation, such as an image frame. Its identity and lifetime are distinct from a temporary rendering operation.
_Avoid_: Summary text, disposable preview

**Bitmap archive**:
A representation of selected conversation text as image frames intended for a vision-capable model. Bitmap encoding alone does not imply that every part of the original history was retained.
_Avoid_: Lossless memory, text summary

**Memory scope**:
An explicitly selected collection of remembered information available to one or more conversations. A fresh session does not implicitly join another session's memory scope.
_Avoid_: Global memory, current context

**Summary tree**:
A hierarchical memory representation whose leaves refer to original material and whose higher nodes summarize larger spans. A bounded tree view can represent more history than fits verbatim in one context.
_Avoid_: Flat rolling summary, canonical history

**Recall**:
The retrieval of finer-grained or original material represented by a memory view. Recall is distinct from automatically including all stored material in a model call.
_Avoid_: Context assembly, summarization

## Continual harness

**Continual harness**:
An optional source of persistent supplemental knowledge and reusable agent behavior, including memories, prompt notes, skill descriptions, and subagent specifications. It is distinct from the agent's mandatory execution machinery.
_Avoid_: Entire agent runtime, immutable base prompt

**Harness record**:
A durable item of supplemental knowledge or reusable behavior owned by the continual harness within a declared scope.
_Avoid_: Session message, plugin registration

**Refinement**:
An operation that proposes or applies evidence-backed changes to harness records. It is distinct from conversation compaction and from changing the immutable base instructions.
_Avoid_: Compaction, self-rewriting core
