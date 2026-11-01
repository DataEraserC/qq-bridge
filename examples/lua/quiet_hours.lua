-- 夜间静默唤醒策略（示例）。
--
-- 用法：
--   1. 让桥装上 LuaLoader：ACB_LUA_DIR=<仓库>/examples/lua（见 examples/README.md）
--   2. 在订阅里写：
--        [sessions.subscriptions.wake]
--        provider = "lua.quiet_hours"
--
-- 规则：本地 23:00–08:00 不主动唤醒；@ 我、回复我仍然醒。
-- 时间按 UTC+8 计算（context.now 是 Unix 毫秒）。
-- 这个示例只保留「@ / 回复 / 攒条数」三件事，其它条件请照这个形状自己补。

return {
  extension = "wake",
  decide = function(options, context)
    if context.at_self or context.reply_to_self then
      return { decision = "wake", reason = "冲我来的" }
    end

    local hour = math.floor((context.now / 3600000 + 8) % 24)
    if hour >= 23 or hour < 8 then
      return "stay"
    end

    local threshold = options.accumulate or 5
    if context.unread >= threshold then
      return { decision = "wake", reason = "白天攒够 " .. threshold .. " 条" }
    end
    return "stay"
  end,
}
