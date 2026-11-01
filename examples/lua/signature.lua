-- 发送前签名钩子（示例）。
--
-- 用法：
--   1. 让桥装上 LuaLoader：ACB_LUA_DIR=<仓库>/examples/lua
--   2. 在会话里写：
--        [sessions.outbound]
--        before = ["lua.signature"]
--
-- 规则：普通回复在末尾追加一句签名；失败通知（origin = "turn_failure"）不动，
-- 免得错误通知也被加上签名。返回 nil 表示不改，返回元素数组表示整体替换。

return {
  extension = "send",
  before = function(context, elements)
    if context.origin ~= "message" then
      return nil
    end
    elements[#elements + 1] = { type = "text", text = "\n——来自小助手" }
    return elements
  end,
}
