-- 只读访问策略（示例）。
--
-- 用法：
--   1. 让桥装上 LuaLoader：ACB_LUA_DIR=<仓库>/examples/lua
--   2. 在会话里写：
--        [sessions.access]
--        provider = "lua.allow_read_only"
--
-- 规则：读（历史、事件、笔记、只读平台方法）放行；发送和连接级操作拒绝。
-- 破坏性方法另有连接的 allow_destructive 单独把关，这个策略不能替它放行。

return {
  extension = "access",
  decide = function(_, request)
    if request.action == "read" then
      return "allow"
    end
    return "deny"
  end,
}
