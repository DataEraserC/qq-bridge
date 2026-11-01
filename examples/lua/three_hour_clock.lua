-- 长时间没有唤醒就开一轮检查（配合 wake_policy.lua 的 idle_hours）。
--
-- 用法：
--   1. ACB_LUA_DIR 指向这个目录
--   2. 每个订阅写：
--        [sessions.subscriptions.clock]
--        provider = "lua.three_hour_clock"
--        [sessions.subscriptions.clock.options]
--        idle_hours = 3
--
-- core 每秒调一次 is_due；只有距上次唤醒（任意原因开轮）超过 idle_hours 才返回
-- true。返回 false 不做任何事；脚本报错时 core 按「不触发」兜底。

return {
  extension = "clock",
  is_due = function(options, context)
    local idle_hours = options.extra.idle_hours
    if idle_hours == nil then
      idle_hours = 3
    end
    if idle_hours <= 0 then
      return false
    end

    local last = context.last_wake_at or context.now
    return context.now - last >= idle_hours * 3600000
  end,
}
