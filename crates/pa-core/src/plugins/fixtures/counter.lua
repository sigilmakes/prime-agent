-- A second plugin has its own VM and independent upvalues.
local count = 100
return {
    exports = {
        {name = "count", run = function(input, ctx)
            count = count + 1
            return count
        end},
        {name = "wait", run = function(input, ctx)
            return ctx.call("wait", input)
        end},
        {name = "busy", run = function(input, ctx)
            ctx.emit("ready")
            while true do count = count + 1 end
        end, mode = "streaming"},
        {name = "environment", run = function(input, ctx)
            return {io = io ~= nil, os = os ~= nil, package = package ~= nil,
                require = require ~= nil, loadfile = loadfile ~= nil}
        end},
        {name = "retain", run = function(input, ctx)
            retained = ctx
            return "saved"
        end},
        {name = "reuse", run = function(input, ctx)
            return retained.call("wait", input)
        end}
    }
}
