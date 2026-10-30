#!/usr/bin/env bash
# start-drivers.sh —— 起两个 acb 驱动进程（由 acb-up.sh 通过 systemd 单元调用）。
#
# 每个驱动是一个进程干三件事：连桥（环回口）+ 起一个本地 MCP 端点 + 驱动一个 dsh 会话。
# 一个 preset 只挂一个 MCP，模型在某个会话里就只看到那一套工具，不会拿着「另一套没票的
# 工具」去试错（ADR-0019）。
#
# 会话令牌不写死在这里：从 config.toml 读。否则改配置得同步改脚本，两边一漂就握手失败。
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
CONFIG=$REPO/config.toml
DRIVER=$REPO/harnesses/dsh/src/driver.mjs

NODE=${NODE:-$(command -v node || echo /etc/profiles/per-user/nixos/bin/node)}
[[ -x $NODE ]] || { echo "找不到 node：$NODE" >&2; exit 1; }

# dsh 每次重启都换 launch token（只打在它自己的启动输出里）。驱动不去猜它，
# 走离线铸造 cookie（~/.dsh/.credentials.yaml 里的 browser-session 密钥），
# 见 dsh-client.mjs 的 ensureAuth：能铸造就优先铸造，所以这里不传 DSH_TOKEN。

# 从 config.toml 的 [[sessions]] 块里取某个会话的 token。
session_token() {
  awk -v want="\"$1\"" '
    /^\[\[sessions\]\]$/ { ins=1; hit=0; next }
    /^\[\[/              { ins=0; next }
    ins && /^id = /      { hit = (index($0, want) > 0); next }
    ins && hit && /^token = / { gsub(/^token = "|"[[:space:]]*$/, ""); print; exit }
  ' "$CONFIG"
}

# 会话 id : MCP 端口 : preset 名
# preset 名必须和 harnesses/dsh/dsh-mcp.patch.yml 里注册的一致，否则 dsh 会报
# Unknown agent preset，连 session resume 都失败。
SESSIONS=(
  "dsh:3101:acb-dsh"
  "dsh-group2:3102:acb-dsh-group2"
)

for entry in "${SESSIONS[@]}"; do
  IFS=: read -r sid port preset <<< "$entry"
  token=$(session_token "$sid")
  if [[ -z $token ]]; then
    echo "config.toml 里没有会话 $sid 的 token，跳过" >&2
    continue
  fi
  echo "起驱动：会话 $sid → MCP $port → preset $preset"
  env DSH_HOME="${DSH_HOME:-$HOME/.dsh}" \
    ACB_ENDPOINT=127.0.0.1:7700 \
    ACB_TOKEN="$token" \
    ACB_HARNESS=dsh \
    ACB_PRESET="$preset" \
    ACB_MCP_PORT="$port" \
    DSH_BASE_URL=http://127.0.0.1:3080 \
    "$NODE" "$DRIVER" &
done

wait
