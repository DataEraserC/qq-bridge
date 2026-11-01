-- 失败通知策略（示例）。
--
-- 用法：
--   1. 让桥装上 LuaLoader：ACB_LUA_DIR=<仓库>/examples/lua
--   2. 在会话里写：
--        [sessions.turn_error]
--        provider = "lua.timeout_log_only"
--        [sessions.turn_error.options]
--        ops_room = "qq:group/ops"   # 可选：会话没配动作时发到这里
--
-- 规则：超时类失败只记日志，不往群里发消息；其它失败沿用会话自己的
-- on_turn_error；会话没配动作时用 options.extra.ops_room（没配就回退全局默认）。
-- 返回空数组表示「什么都不做」；脚本报错时 core 回退默认选择。

return {
  extension = "turn_error",
  decide = function(request, options)
    local timeout = string.find(request.error, "timeout", 1, true)
      or string.find(request.error, "超时", 1, true)
    if timeout then
      return { { kind = "log" } }
    end

    if #request.actions > 0 then
      return request.actions
    end
    if options.extra.ops_room then
      return { { kind = "send", room = options.extra.ops_room } }
    end
    return request.default_actions
  end,
}
