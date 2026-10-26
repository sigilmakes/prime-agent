-- The default rolling view is explicit plugin policy, not canonical-history storage.
local policy = config.policy or "rolling"
assert(policy == "rolling" or policy == "full", "context policy must be rolling or full")
local compactors = config.compactors or {
    "server_compaction/server", "snapcompact/snapcompact", "standard_compaction/standard"
}
assert(type(compactors) == "table", "compactors must be an array")
for key, route in pairs(compactors) do
    assert(type(key) == "number" and key >= 1 and key <= #compactors and key % 1 == 0,
        "compactors must be a dense array")
    assert(type(route) == "string", "compactor routes must be strings")
end
local function accepts_archive(archive)
    return type(archive) == "table" and archive.version == 1
        and archive.strategy == "default_context/rolling"
end
return {context_strategies = {{name = "rolling", context_data = "refs_v1", run = function(input)
    if input.stage == "restore" then
        -- creatorGeneration is durable provenance, not current invocation authority.
        return {accepted = accepts_archive(input.archive)}
    end
    if input.stage == "assemble" then
        assert(input.archive == nil or input.archive == null or accepts_archive(input.archive),
            "unsupported context archive schema")
        return {messages = input.messages}
    end
    assert(input.stage == "prepare", "unsupported context strategy stage")
    return {compactors = compactors, policy = policy}
end}}}
