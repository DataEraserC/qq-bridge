#!/usr/bin/env bash
# acb-up.sh —— 起 acb 全链路。顺序固定：dsh → 桥 → 驱动。
#
# 为什么是 systemd 用户服务而不是 nohup/后台 shell：常驻进程绝不能挂在 AI 会话里。
# 会话沙箱一撤，进程活着却开不出新 socket（桥报 os error 38、驱动报 connect ENOSYS），
# 表现就是「进程都在、群里没人理」。挂到 systemd 用户服务下就不依赖任何会话。
#
# 为什么脚本放在仓库而不是 ~/qqab-scratch：scratch 在 btrfs 的 /root 子卷，重启即清空；
# 仓库在 @persistent 子卷。部署层必须可重建，否则每次重启都得凭记忆重拼命令。
#
# 用法：deploy/acb-up.sh
# 密钥：deploy/acb.env（不入库），只有一个 NEW_API_Y9000P2025_OPENAI_RESPONSES_API_KEY。
set -uo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
LOGDIR=${ACB_LOG_DIR:-$HOME/qqab-scratch/logs}
ENV_FILE=$HERE/acb.env
PATCH=$REPO/harnesses/dsh/dsh-mcp.patch.yml
BRIDGE=$REPO/result/bin/acb-bridge

mkdir -p "$LOGDIR"

# ---- 前置检查 ---------------------------------------------------------------
fail() { echo "✗ $*" >&2; exit 1; }

[[ -x $BRIDGE ]] || fail "没有 $BRIDGE，先跑：nix build .#acb-bridge"
[[ -f $PATCH ]] || fail "没有 $PATCH"
[[ -f $REPO/config.toml ]] || fail "没有 $REPO/config.toml"

# dsh-real 不是 dsh：`dsh` 是 dsh-seed-profile 的包装器，会按模板重建 profile，
# 把 --patch 注入的东西洗掉；dsh-real 才是直连二进制。用 dsh 所在目录去拼，
# 这样 dsh 升级版本后不用改这里的 nix store 路径。
DSH_BIN=${DSH_BIN:-}
if [[ -z $DSH_BIN ]]; then
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

# 已经 active 的单元用 restart 才能换掉旧的启动参数。
start_unit() {
  local unit=$1; shift
  systemctl --user stop "$unit" 2>/dev/null
  systemd-run --user --unit="$unit" \
    --property=Restart=always --property=RestartSec=5 \
    "$@" 2>&1 | sed 's/^/    /'
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

echo "→ 起 dsh（3080）"
dsh_env=()
if [[ -f $ENV_FILE ]]; then
  dsh_env=(--property=EnvironmentFile="$ENV_FILE")
else
  echo "    ⚠ 没有 $ENV_FILE，dsh 会因缺 API key 报 NO_ADAPTER（见 deploy/acb.env.example）" >&2
fi
start_unit dsh-acb \
  --property=WorkingDirectory="$HOME" \
  --property=StandardOutput=append:"$LOGDIR/dsh.log" \
  --property=StandardError=append:"$LOGDIR/dsh.log" \
  "${dsh_env[@]}" \
  "$DSH_BIN" --profile nix-web-acb --patch "$PATCH" --no-open --port 3080
wait_port 3080 "dsh"

echo "→ 起桥（7700 + 控制台 3100）"
start_unit acb-bridge \
  --property=WorkingDirectory="$REPO" \
  --property=StandardOutput=append:"$LOGDIR/bridge.log" \
  --property=StandardError=append:"$LOGDIR/bridge.log" \
  "$BRIDGE" --config config.toml
wait_port 7700 "桥"

echo "→ 起驱动（3101 / 3102）"
start_unit acb-drivers \
  --property=WorkingDirectory="$HERE" \
  --property=StandardOutput=append:"$LOGDIR/drivers.log" \
  --property=StandardError=append:"$LOGDIR/drivers.log" \
  /run/current-system/sw/bin/bash "$HERE/start-drivers.sh"

sleep 3

# ---- 结果 -------------------------------------------------------------------
echo
echo "=== 结果 ==="
printf '%-8s %s\n' "dsh"    "$(systemctl --user is-active dsh-acb    2>/dev/null || true)"
printf '%-8s %s\n' "桥"     "$(systemctl --user is-active acb-bridge 2>/dev/null || true)"
printf '%-8s %s\n' "驱动"   "$(systemctl --user is-active acb-drivers 2>/dev/null || true)"

token_line=$(grep -o 'http://127\.0\.0\.1:3080/?token=[A-Za-z0-9_-]*' "$LOGDIR/dsh.log" 2>/dev/null | tail -1)
[[ -n $token_line ]] && echo "dsh UI： $token_line"

echo -n "桥自述： "
curl -s --max-time 5 -H "Authorization: Bearer $(grep -A3 '^\[console\]' "$REPO/config.toml" | grep -o 'token *= *"[^"]*"' | cut -d'"' -f2)" \
  http://127.0.0.1:3100/api/status || echo "（取不到，看日志）"
echo
echo "日志目录：$LOGDIR"
