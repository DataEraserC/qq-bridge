-- 通用唤醒策略：@、poke、未读条数、长时间未唤醒。
--
-- 用法：
--   1. ACB_LUA_DIR 指向这个目录（见 examples/README.md）
--   2. 每个订阅写：
--        [sessions.subscriptions.wake]
--        provider = "lua.wake_policy"
--        [sessions.subscriptions.wake.options]
--        at_mention = true
--        poke = true
--        reply = true
--        accumulate = 30
--        idle_hours = 3
--
-- 所有参数都在 options.extra 里，代码不写死；未写的参数用下面的默认值。
-- 三小时的「静默唤醒」还要配 clock 策略（同一个 idle_hours），因为唤醒端点
-- 只在有入站记录时被调用，没人说话时不会自己触发。

local function has_tag(tags, wanted)
  for _, tag in ipairs(tags or {}) do
    if tag == wanted then
      return true
    end
  end
  return false
end

return {
  extension = "wake",
  decide = function(options, context)
    local extra = options.extra or {}

    -- @ 到我
    if extra.at_mention ~= false and context.at_self then
      return { decision = "wake", reason = "@ 到我" }
    end

    -- 回复我（可选，默认关闭）
    if extra.reply and context.reply_to_self then
      return { decision = "wake", reason = "回复了我" }
    end

    -- 叫名字（可选，默认关闭）
    if extra.name_mention then
      for _, name in ipairs(extra.names or {}) do
        if name ~= "" and context.text:find(name, 1, true) then
          return { decision = "wake", reason = "叫了我的名字" }
        end
      end
    end

    -- 平台事件：poke（默认开启）；标签列表也来自配置
    if extra.poke ~= false then
      local poked = context.event == "poke"
      for _, tag in ipairs(extra.poke_tags or { "poke" }) do
        if has_tag(context.tags, tag) then
          poked = true
        end
      end
      if poked then
        return { decision = "wake", reason = "被拍了一下" }
      end
    end

    -- 未读（未投递）条数达到阈值
    local threshold = extra.accumulate
    if threshold == nil then
      threshold = 30
    end
    if threshold > 0 and context.unread >= threshold then
      return {
        decision = "wake",
        reason = "未读消息达到 " .. threshold .. " 条",
      }
    end

    -- 距上次唤醒超过 idle_hours（没人说话的情况由 clock 策略补）
    local idle_hours = extra.idle_hours
    if idle_hours == nil then
      idle_hours = 3
    end
    if idle_hours > 0 then
      local last = context.last_wake_at or context.now
      if context.now - last >= idle_hours * 3600000 then
        return {
          decision = "wake",
          reason = "距上次唤醒超过 " .. idle_hours .. " 小时",
        }
      end
    end

    return "stay"
  end,
}
