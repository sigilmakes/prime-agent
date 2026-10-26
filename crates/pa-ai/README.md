# pa-ai

Provider APIs and model registry.

## Scope
Provider trait + per-provider streaming clients (anthropic, openai-completions/responses, google, bedrock, mistral, azure, prime-inference), model registry/resolution, usage accounting, stream-failure retry, provider-error shapes (per-SDK user-facing texts, diagnostic error names, connection-error profiles), bedrock transport selection (h2c prior-knowledge HTTP/2 cleartext, h2-preferred TLS ALPN, the http1 AWS_BEDROCK_FORCE_HTTP1/proxy mode), overflow handling, JSON repair parsing, faux provider for tests.

## Non-goals
No agent loop, no tool execution, no session state, no UI. Receives/returns `pa-types` messages.

## Public API
`Provider` trait, `ProviderRegistry`, model lookup/resolution, faux provider. `compaction` owns explicit route-bound server compaction, durable native archives, fresh portable Responses conversion, and the opaque `NativeResponsesInput` handoff to `StreamOptions`. Native replay replaces the complete input array; it does not convert or summarize the retained prefix. Provider adapters must explicitly opt in to preserve native input. `compaction::CompactOptions::headers` accepts caller-resolved inference headers; its explicit bearer token wins over any Authorization header. `utils::tls::http_client_builder` lets caller-owned HTTP transports apply the shared extra-CA trust configuration. Per-provider internals are `pub(crate)`.

## Depends on
pa-types (one-way).

## Opt-in event delivery

`StreamOptions.event_buffer_capacity: Some(n)` requests async event-count backpressure
(`n > 0`). `None` preserves legacy unbounded delivery. Builtin transports and Faux
advertise `Provider::supports_bounded_event_delivery`; custom providers default to
unsupported and the facade rejects unsupported requests before dispatch.

Every emitted event awaits capacity, including multiple events in one SSE chunk or
block-finalization loop. Dropping a bounded reader retires its producer. Cancellation
unwinds the transport before terminal delivery waits for room. `result()` consumes
the reader and switches to completion-only mode, so `complete` and result-only calls
never wait for a full queue they cannot drain. Ordinary iteration retains every event
in order with exactly one terminal event.

This bounds the queued event count only. Event payloads, accumulated assistant output,
HTTP chunks and incomplete SSE frames can still be large; this is not a byte-memory
or frame-size limit.
