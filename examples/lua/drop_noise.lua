-- 丢弃噪声的入站策略（示例）。
--
-- 用法：
--   1. 让桥装上 LuaLoader：ACB_LUA_DIR=<仓库>/examples/lua
--   2. 在连接上写：
--        [connections.receive]
--        provider = "lua.drop_noise"
--
-- 规则：正文以 "!!" 开头的消息不记流水；其它照收。
-- 被丢掉的记录不写流水、不叫醒人，也不会让水位出现空洞。

return {
  extension = "receive",
  decide = function(request)
    if string.sub(request.text, 1, 2) == "!!" then
      return "drop"
    end
    return "record"
  end,
}
