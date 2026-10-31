#!/usr/bin/env bash
# acb-up.sh —— 起 acb 全链路。顺序固定：dsh → 桥 → 驱动 → opencode keeper。
#
# 常驻进程绝不能挂在 AI 会话里：会话沙箱一撤，进程活着却开不出新 socket。
# 这里用 **仓库里的持久 systemd 单元**（deploy/systemd/），只在
# ~/.config/systemd/user/ 放指向仓库的符号链接。单元文件和脚本都在仓库里，
# 仓库在持久子卷；日志写到仓库 state/logs/，不再用 ~/qqab-scratch。
#
# 用法：deploy/acb-up.sh
# 密钥：deploy/acb.env（不入库），只有一个 NEW_API_Y9000P2025_OPENAI_RESPONSES_API_KEY。
set -uo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
LOGDIR=${ACB_LOG_DIR:-$REPO/state/logs}
UNITDIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
BRIDGE=$REPO/result/bin/acb-bridge
UNITS=(dsh-acb.service acb-bridge.service acb-drivers.service acb-opencode-keeper.service)

mkdir -p "$LOGDIR" "$UNITDIR"

# ---- 前置检查 ---------------------------------------------------------------
fail() { echo "✗ $*" >&2; exit 1; }

[[ -x $BRIDGE ]] || fail "没有 $BRIDGE，先跑：nix build .#acb-bridge"
[[ -f $REPO/harnesses/dsh/dsh-mcp.patch.yml ]] || fail "没有 dsh-mcp.patch.yml"
[[ -f $REPO/config.toml ]] || fail "没有 $REPO/config.toml"
[[ -f $HERE/start-dsh.sh ]] || fail "没有 deploy/start-dsh.sh"
[[ -f $HERE/start-drivers.sh ]] || fail "没有 deploy/start-drivers.sh"
[[ -f $HERE/opencode-keeper.sh ]] || fail "没有 deploy/opencode-keeper.sh"

# dsh-real 不是 dsh：`dsh` 是 dsh-seed-profile 的包装器，会按模板重建 profile，
# 把 --patch 注入的东西洗掉；dsh-real 才是直连二进制。这里只做前置检查，
# 真正的解析在 start-dsh.sh（dsh 升级后不用改单元文件里的 store 路径）。
if [[ -z ${DSH_BIN:-} ]]; then
  dsh_wrapper=$(command -v dsh || true)
  [[ -n $dsh_wrapper ]] || fail "PATH 里没有 dsh，没法定位 dsh-real"
  DSH_BIN="$(dirname "$(readlink -f "$dsh_wrapper")")/dsh-real"
fi
[[ -x $DSH_BIN ]] || fail "找不到可执行的 dsh-real：$DSH_BIN"

# 3080 被非托管进程占着时直接停下，免得起出两个 dsh 抢端口。
if ss -tln 2>/dev/null | grep -qE '127\.0\.0\.1:3080|0\.0\.0\.0:3080|\*:3080'; then
  if ! systemctl --user is-active --quiet dsh-acb 2>/dev/null; then
    fail "3080 已被别的进程占用（不是 dsh-acb 单元），先处理掉它"
  fi
fi

# ---- 安装仓库里的单元文件 ---------------------------------------------------
# 先把旧的 transient / 旧单元停掉，避免同名单元拦着新文件生效。
echo "→ 停旧单元（如果有）"
for unit in acb-opencode-keeper.service acb-drivers.service acb-bridge.service dsh-acb.service; do
  systemctl --user stop "$unit" 2>/dev/null || true
done

echo "→ 链接仓库单元到 $UNITDIR"
for unit in "${UNITS[@]}"; do
  ln -sf "$HERE/systemd/$unit" "$UNITDIR/$unit"
done
systemctl --user daemon-reload

echo "→ enable"
systemctl --user enable "${UNITS[@]}" >/dev/null

start_unit() {
  local unit=$1 label=$2 port=$3
  echo "→ 起 $label"
  systemctl --user restart "$unit"
  if [[ -n $port ]]; then
    wait_port "$port" "$label" || true
  fi
  sleep 1
}

wait_port() {
  local port=$1 label=$2 n=0
  while (( n < 40 )); do
    ss -tln 2>/dev/null | grep -q ":$port " && { echo "    $label 已就绪（$port）"; return 0; }
    sleep 0.5; (( n++ ))
  done
  echo "    ⚠ $label 30 秒内没在 $port 上监听，看日志：$LOGDIR" >&2
  return 1
}

start_unit dsh-acb.service "dsh（3080）" 3080
start_unit acb-bridge.service "桥（7700 + 控制台 3100）" 7700
start_unit acb-drivers.service "驱动（3101 / 3102）" ""
start_unit acb-opencode-keeper.service "opencode keeper" ""

sleep 3

# ---- 结果 -------------------------------------------------------------------
echo
echo "=== 结果 ==="
printf '%-24s %s\n' "dsh"    "$(systemctl --user is-active dsh-acb 2>/dev/null || true)"
printf '%-24s %s\n' "桥"     "$(systemctl --user is-active acb-bridge 2>/dev/null || true)"
printf '%-24s %s\n' "驱动"   "$(systemctl --user is-active acb-drivers 2>/dev/null || true)"
printf '%-24s %s\n' "opencode" "$(systemctl --user is-active acb-opencode-keeper 2>/dev/null || true)"

token_line=$(grep -o 'http://127\.0\.0\.1:3080/?token=[A-Za-z0-9_-]*' "$LOGDIR/dsh.log" 2>/dev/null | tail -1)
[[ -n $token_line ]] && echo "dsh UI： $token_line"

echo -n "桥自述： "
curl -s --max-time 5 -H "Authorization: Bearer $(grep -A3 '^\[console\]' "$REPO/config.toml" | grep -o 'token *= *"[^"]*"' | cut -d'"' -f2)" \
  http://127.0.0.1:3100/api/status || echo "（取不到，看日志）"
echo
echo "日志目录：$LOGDIR"
echo "单元文件：$HERE/systemd/（systemd 侧是 $UNITDIR 的符号链接）"
