-- A small stateful plugin, exercised only through PluginHost.
local count = 0
return {
    exports = {
        {name = "count", run = function(input, ctx)
            count = count + 1
            return count
        end},
        {name = "bad_emit", run = function(input, ctx)
            ctx.emit("not allowed")
        end},
        {name = "bad_result", run = function(input, ctx)
            return function() end
        end}
    },
    commands = {
        {name = "remember", mode = "streaming", run = function(input, ctx)
            ctx.emit({stage = "started"})
            local result = ctx.call("auxiliary", input)
            ctx.emit({stage = "completed"})
            return result
        end}
    },
    hooks = {
        {name = "annotate", run = function(input, ctx)
            return {text = "ledger:" .. input.text}
        end}
    }
}
