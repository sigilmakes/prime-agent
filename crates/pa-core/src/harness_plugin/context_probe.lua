-- Run with Lua 5.4: lua context_probe.lua policy.lua
config={}; null={}
local policy=assert(loadfile(assert(arg[1])))()
local hook
for _, item in ipairs(policy.hooks) do if item.name=="context_contribution" then hook=item.run end end
local records, scopes, enumerations={}, {}, {}
local ctx={call=function(name,input)
    if name=="session.info" then return {session="fixture"} end
    if name=="harness.store.read" then return {value={scopes=scopes}} end
    if name=="harness.store.enumerate" then
        enumerations[input.scope]=(enumerations[input.scope] or 0)+1
        return records[input.scope] or {}
    end
    error("unexpected IO: "..name)
end}
local function row(id,content,title)
    return {identity={kind="memory",id=id},value={content=content,title=title or id}}
end
local function render(text)
    enumerations={}
    return hook({recent_texts={text or ""}},ctx).instructions
end
records["session:fixture"]={}
for i=1,100 do table.insert(records["session:fixture"],row(string.format("%03d",i),string.rep("界",16000))) end
local output=render()
assert(#output<8192, #output)
assert(utf8.len(output))
assert(output:find("94 omitted",1,true))
assert(output:find('id="001"',1,true) and output:find('id="006"',1,true))
assert(not output:find('id="007"',1,true))
assert(#records["session:fixture"][1].value.content==48000)
-- Every selected scope participates; duplicate membership neither enumerates nor
-- renders twice, and identical IDs in different scopes remain distinguishable.
scopes={"other","other","session:fixture"}
records["session:fixture"]={row("same","old quantum evidence")}
records.other={row("same","older 東京 evidence"),row("a","irrelevant newer"),row("b","irrelevant newer"),row("c","irrelevant newer"),row("d","irrelevant newer"),row("e","irrelevant newer"),row("f","irrelevant newer")}
output=render("東京 quantum")
assert(enumerations.other==1 and enumerations["session:fixture"]==1)
assert(output:find('scope="other", kind=memory, id="same"',1,true))
assert(output:find('scope="session:fixture", kind=memory, id="same"',1,true))
assert(output:find("older 東京 evidence",1,true))
assert(output==render("東京 quantum"))
-- Per-kind IDF: rare quantum outranks common session despite common title overlap.
scopes={}
records["session:fixture"]={row("a","session","session"),row("b","session","session"),row("c","session","session"),row("d","session","session"),row("e","session","session"),row("f","session","session"),row("z","quantum")}
output=render("session quantum")
assert(output:find('id="z"',1,true))
print("context policy probe passed; bounded UTF-8 snippets, IDF/CJK, scopes, stable ties, raw preservation")
