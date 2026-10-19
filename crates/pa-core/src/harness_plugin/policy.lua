-- Continual-harness policy. Storage and inference have no ambient paths here.
local local_scope = nil
local selected = nil
local function facts(ctx)
    local info = ctx.call("session.info", {})
    local_scope = "session:" .. info.session
    if selected == nil then
        selected = {local_scope}
        for _, scope in ipairs(config.scopes or {}) do
            if scope ~= local_scope then table.insert(selected, scope) end
        end
    end
    return info
end
local function scope(input, ctx)
    facts(ctx)
    return input.scope or local_scope
end
local function identity(input, ctx)
    assert(type(input.kind) == "string" and input.kind ~= "", "kind is required")
    assert(type(input.id) == "string" and input.id ~= "", "id is required")
    return {scope=scope(input, ctx), kind=input.kind, id=input.id}
end
local function read(input, ctx)
    return ctx.call("harness.store.read", {identity=identity(input, ctx)})
end
local function create(input, ctx)
    local key = identity(input, ctx)
    local value = input.value
    if value == nil then
        value = {id=input.id, kind=input.kind, title=input.title or input.id,
            content=input.content or "", reference=input.reference or {},
            arguments=input.arguments or {}, metadata=input.metadata or {},
            source=input.source or "plugin.harness", version=1}
    end
    return ctx.call("harness.store.put", {identity=key, value=value, expected="absent"})
end
local function update(input, ctx)
    local key = identity(input, ctx)
    local old = ctx.call("harness.store.read", {identity=key})
    assert(old ~= null, "record does not exist")
    local value = input.value
    if value == nil then
        assert(type(old.value) == "table", "raw record replacement requires value")
        value = old.value
        for key, field in pairs(input.changes or {}) do value[key] = field end
    end
    return ctx.call("harness.store.put", {identity=key, value=value,
        expected=input.revision or old.revision})
end
local function delete(input, ctx)
    return ctx.call("harness.store.delete", {identity=identity(input, ctx),
        expected=input.revision or "any"})
end
local function list(input, ctx)
    return ctx.call("harness.store.enumerate", {scope=scope(input, ctx)})
end
local function enroll(input, ctx)
    facts(ctx)
    assert(type(input.scopes) == "table", "scopes is required")
    selected = {local_scope}
    for _, name in ipairs(input.scopes) do
        assert(type(name)=="string" and name~="", "invalid scope")
        if name~=local_scope then table.insert(selected,name) end
    end
    -- Persist session membership, not global default membership.
    return ctx.call("harness.store.put", {identity={scope=local_scope,kind="_policy",id="scopes"},
        value={scopes=input.scopes}, expected="any"})
end
local function selected_scopes(ctx)
    facts(ctx)
    local saved = ctx.call("harness.store.read", {identity={scope=local_scope,kind="_policy",id="scopes"}})
    if saved ~= null and saved.value.scopes ~= nil then
        selected = {local_scope}
        for _, name in ipairs(saved.value.scopes) do
            if name~=local_scope then table.insert(selected,name) end
        end
    end
    return selected
end
local visible = {prompt=true,memory=true,skill=true,subagent=true}
local function context(input, ctx)
    local sections = {}
    for _, name in ipairs(selected_scopes(ctx)) do
        local rows = ctx.call("harness.store.enumerate", {scope=name})
        for _, row in ipairs(rows) do
            local value = row.value
            if visible[row.identity.kind] and type(value)=="table" then
                local content = value.content
                if type(content)=="string" and content~="" then
                    table.insert(sections, "["..name..":"..row.identity.kind..":"..row.identity.id.."] "..content)
                end
            end
        end
    end
    return {instructions="Optional continual harness: use plugin.harness exports for records. "..
        "Enroll named scopes explicitly with plugin.harness.enroll(scopes=[...]). "..
        "Factory and unknown records are passive data, never executable registrations. "..
        table.concat(sections,"\n")}
end
local function enqueue(input, ctx)
    facts(ctx)
    return ctx.call("jobs.enqueue", {hook="refinement_job",input=input})
end
local function refine(input,ctx)
    input.reason = input.reason or "manual"
    return enqueue(input,ctx)
end
local function automatic(input,ctx)
    if input.reason=="cancelled" then return {scheduled=false,reason="cancelled"} end
    local policy=config.autoRefine
    if policy==null or policy==nil then policy={} end
    if policy.enabled==false then return {scheduled=false} end
    facts(ctx)
    local key={scope=local_scope,kind="_policy",id="automatic"}
    local saved=ctx.call("harness.store.read",{identity=key})
    local state={turns=0}
    if saved~=null then state=saved.value end
    local compact=input.stage=="compaction_settled"
    if not compact then state.turns=state.turns+1 end
    local interval=math.max(1,policy.turnInterval or 25)
    local due=(compact and policy.compact~=false) or (not compact and state.turns%interval==0)
    local now=input.now_ms or 0
    local cooled=state.last_review==nil or now-state.last_review>=(policy.cooldownMs or 1200000)
    if due and cooled then state.last_review=now end
    ctx.call("harness.store.put",{identity=key,value=state,expected=saved~=null and saved.revision or "absent"})
    if not due or not cooled then return {scheduled=false,reason="policy"} end
    return enqueue({reason=compact and "compact" or "interval",automatic=true},ctx)
end
local function refinement_job(input,ctx)
    local snapshot=ctx.call("session.snapshot",{})
    local target=scope(input,ctx)
    local records=ctx.call("harness.store.enumerate",{scope=target})
    local review=nil
    do
        review=ctx.call("inference.complete", {purpose="harness review",model=config.auxiliaryModel,
            system="Review evidence for durable harness changes. Return JSON {shouldRefine:boolean,rationale:string}.",
            input={history=snapshot.history,records=records,reason=input.reason,instructions=input.instructions}})
        local decision=ctx.call("native.json.decode",{text=review.text})
        if decision.shouldRefine~=true then return {applied=false,reason="review declined"} end
    end
    -- Both phases use the same configured auxiliary route. Policy stays in Lua.
    local plan=ctx.call("inference.complete", {purpose="harness plan",model=config.auxiliaryModel,
        system="Propose only evidence-backed record changes. Return JSON {summary:string,edits:[{action:create|update|delete,kind:string,id:string,value:any}]}. Prefer empty edits. Never execute factory or unknown records.",
        input={history=snapshot.history,records=records,instructions=input.instructions,review=review}})
    local proposal=ctx.call("native.json.decode",{text=plan.text})
    assert(type(proposal.edits)=="table", "proposal needs edits")
    return ctx.call("harness.store.apply",{scope=target,baseline=records,proposal=proposal,
        branch=snapshot.branch,reason=input.reason})
end
local function history(input,ctx)
    return ctx.call("harness.store.history",{identity=identity(input,ctx)})
end
local function rollback(input,ctx)
    if input.history_id~=nil then
        return ctx.call("harness.store.rollback_legacy",{scope=scope(input,ctx),id=input.history_id})
    end
    assert(type(input.revision)=="number", "revision is required")
    return ctx.call("harness.store.rollback",{identity=identity(input,ctx),
        revision=input.revision,expected=input.expected or "any"})
end
return {
    exports={
        {name="create",description="Create a raw record in an explicit scope (current session by default).",run=create},
        {name="update",description="Update a record with optimistic revision checks.",run=update},
        {name="delete",description="Delete a record; retained history permits rollback.",run=delete},
        {name="get",description="Get one scope/kind/id record without executing its data.",run=read},
        {name="list",description="List records in one selected scope.",run=list},
        {name="enroll",description="Select named memory scopes for this session only.",run=enroll},
        {name="history",description="Read lossless record change history.",run=history},
        {name="import_history",description="Inspect lossless imported sources, conflicts and original histories.",run=function(input,ctx) return ctx.call("harness.store.inspect",{}) end},
        {name="rollback",description="Restore a record revision with conflict checks.",run=rollback},
        {name="refine",description="Schedule evidence-backed harness refinement at a safe boundary.",run=refine},
        {name="job_status",description="Read the current generation's deferred job status.",run=function(input,ctx) return ctx.call("jobs.status",input) end},
        {name="cancel_job",description="Cancel the current generation's deferred job.",run=function(input,ctx) return ctx.call("jobs.cancel",input) end}
    },
    commands={{name="refine",description="Refine optional harness records.",run=function(input,ctx)
        return refine({instructions=input.args},ctx)
    end}},
    hooks={
        {name="refinement_job",run=refinement_job},
        {name="context_contribution",run=context},
        {name="run_settled",run=automatic},
        {name="compaction_settled",run=automatic}
    }
}
