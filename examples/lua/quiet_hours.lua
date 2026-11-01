-- 夜间静默唤醒策略（示例）。
--
-- 用法：
--   1. 让桥装上 LuaLoader：ACB_LUA_DIR=<仓库>/examples/lua（见 examples/README.md）
--   2. 在订阅里写：
--        [sessions.subscriptions.wake]
--        provider = "lua.quiet_hours"
--        [sessions.subscriptions.wake.options]
--        start_hour = 23
--        end_hour = 8
--
-- 规则：本地 start_hour–end_hour 之间不主动唤醒；@ 我、回复我仍然醒。
-- 时间按 UTC+8 计算（context.now 是 Unix 毫秒）。
-- 这个示例只保留「@ / 回复 / 攒条数」三件事，其它条件请照这个形状自己补。

return {
  extension = "wake",
  decide = function(options, context)
    if context.at_self or context.reply_to_self then
      return { decision = "wake", reason = "冲我来的" }
    end

    local hour = math.floor((context.now / 3600000 + 8) % 24)
    local start_hour = options.extra.start_hour or 23
    local end_hour = options.extra.end_hour or 8
    local quiet
    if start_hour >= end_hour then
      quiet = hour >= start_hour or hour < end_hour
    else
      quiet = hour >= start_hour and hour < end_hour
    end
    if quiet then
      return "stay"
    end

    local threshold = options.extra.accumulate or options.accumulate or 5
    if context.unread >= threshold then
      return { decision = "wake", reason = "白天攒够 " .. threshold .. " 条" }
    end
    return "stay"
  end,
}
