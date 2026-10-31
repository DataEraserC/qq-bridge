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
PATCH=$REPO/harnesses/dsh/dsh-mcp.patch.yml

NODE=${NODE:-$(command -v node || echo /etc/profiles/per-user/nixos/bin/node)}
[[ -x $NODE ]] || { echo "找不到 node：$NODE" >&2; exit 1; }

# 模型：以 dsh-mcp.patch.yml 的 agent-default-model 为准，脚本不另存一份。
# 为什么要传给驱动：dsh 把「上次用的模型」记在会话记录里，复用老会话时它优先于默认模型，
# 所以光改补丁不生效——驱动得在每轮开始前对这个会话 selectModel 一次（见 driver.mjs）。
patch_model() {
  awk -v key="$1" '
    /^- id: agent-default-model/ { f = 1; next }
    f && /^- / { f = 0 }
    f && $1 == key ":" { print $2; exit }
  ' "$PATCH"
}

DSH_PROVIDER=$(patch_model provider)
DSH_MODEL=$(patch_model model)
DSH_EFFORT=$(patch_model reasoningEffort)
if [[ -n $DSH_PROVIDER && -n $DSH_MODEL ]]; then
  echo "模型：$DSH_PROVIDER/$DSH_MODEL${DSH_EFFORT:+（$DSH_EFFORT）}"
else
  echo "补丁里没读到 agent-default-model 的 provider/model，驱动不干预模型" >&2
fi

# dsh 每次重启都换 launch token（只打在它自己的启动输出里）。驱动不去猜它，
# 走离线铸造 cookie（~/.dsh/.credentials.yaml 里的 browser-session 密钥），
# 见 dsh-client.mjs 的 ensureAuth：能铸造就优先铸造，所以这里不传 DSH_TOKEN。

# 会话的归组与命名：cwd 决定 dsh UI 里归进哪个 workspace（Workspaces 列表按它分组），
# 标题只在 workspace 第一次被创建时命名，不覆盖已有标题。
# **cwd 必须是给 agent 的独立目录，绝不能指到真实项目**（模型在那个目录里有文件工具，
# 指 nix-config 等于把配置仓库交给群里）。照旧 node 实现的做法：state 下的专用目录。
# 所有驱动都归同一个 workspace，dsh 侧就是一组，标题平台中立。
# 注意：改 ACB_CWD 会让旧会话 id 的 cwd 对不上（dsh 报 belongs to … not …），
# 驱动会自动换新 id 重建（driver.mjs 的冲突兜底），历史上下文留在旧 workspace 里。
ACB_CWD="${ACB_CWD:-$REPO/state/agents}"
ACB_WORKSPACE_TITLE="${ACB_WORKSPACE_TITLE:-ACB}"
mkdir -p "$ACB_CWD"
export ACB_CWD ACB_WORKSPACE_TITLE
echo "dsh 会话归组：$ACB_CWD（workspace 标题：$ACB_WORKSPACE_TITLE）"

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
    ACB_DSH_PROVIDER="$DSH_PROVIDER" \
    ACB_DSH_MODEL="$DSH_MODEL" \
    ACB_DSH_EFFORT="$DSH_EFFORT" \
    "$NODE" "$DRIVER" &
done

wait
