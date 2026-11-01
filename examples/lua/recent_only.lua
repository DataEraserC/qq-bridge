-- 只投最新一段的投递策略（示例）。
--
-- 用法：
--   1. 让桥装上 LuaLoader：ACB_LUA_DIR=<仓库>/examples/lua（见 examples/README.md）
--   2. 在会话里写：
--        [sessions.delivery]
--        provider = "lua.recent_only"
--        [sessions.delivery.options]
--        keep = 20
--
-- 规则：只保留最新 keep 条（默认 20）；更早的当作「跳过」，把编号区间
-- 交给 core 的 omitted，harness 会在轮次开头看到「还有 N 条没投」。
-- 返回 nil 或报错时 core 回退内置「最旧一小段 + 最新一批」。

return {
  extension = "delivery",
  compose = function(options, request)
    local keep_count = options.extra.keep or 20
    if keep_count < 1 then
      keep_count = 1
    end

    local total = #request.records
    if total == 0 then
      return { keep = {} }
    end

    local first_keep = math.max(1, total - keep_count + 1)
    local keep = {}
    for index = first_keep, total do
      keep[#keep + 1] = request.records[index].id
    end

    if first_keep == 1 then
      return { keep = keep }
    end
    return {
      keep = keep,
      omitted = {
        from = request.records[1].id,
        to = request.records[first_keep - 1].id,
        count = first_keep - 1,
      },
    }
  end,
}
