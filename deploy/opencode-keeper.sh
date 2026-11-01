#!/usr/bin/env bash
# opencode-keeper.sh —— acb-opencode-keeper.service 的入口。
#
# opencode 的插件只在「项目被打开过」的 server 里加载，所以用 opencode2 run
# 踢一脚让它 spawn server + 加载 acb 插件 + 连桥。判活标准 = 桥 7700 上的
# ESTAB 连接数（2 条 dsh + 1 条 opencode = 3；ss 每对列两行，所以是 6 行）。
#
# 这个脚本在仓库 deploy/ 下，日志写到仓库 state/logs/；state 与仓库都在持久
# 子卷，不依赖 /home/nixos/qqab-scratch。
set -u
export PATH=/run/current-system/sw/bin:/etc/profiles/per-user/nixos/bin:$PATH

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
SESSION=${ACB_OPENCODE_SESSION:-opencode-1093015329}
STATE_DIR=${ACB_OPENCODE_DIR:-$REPO/state/$SESSION}
LOGDIR=${ACB_LOG_DIR:-$REPO/state/logs}
LOG=$LOGDIR/$SESSION.log
OC=${ACB_OPENCODE_BIN:-/etc/profiles/per-user/nixos/bin/opencode2}

[[ -d $STATE_DIR ]] || { echo "✗ 没有 opencode 状态目录：$STATE_DIR" >&2; exit 1; }
[[ -f $STATE_DIR/env ]] || { echo "✗ 没有 $STATE_DIR/env" >&2; exit 1; }
[[ -x $OC ]] || { echo "✗ 找不到 opencode2：$OC" >&2; exit 1; }

mkdir -p "$LOGDIR"
cd "$STATE_DIR"
# 模型网关钥匙（ACB_MODEL_KEY）给 opencode run 用；{env:ACB_MODEL_KEY} 在配置解析时展开
# shellcheck disable=SC1091
. ./env

boot() {
  local restart_service=${1:-no}
  if [ "$restart_service" = yes ]; then
    echo "[$(date '+%F %T')] 连续缺失，重启 opencode service 再拉起插件" >> "$LOG"
    timeout 60 "$OC" service restart >> "$LOG" 2>&1 || true
    sleep 2
  fi
  echo "[$(date '+%F %T')] 连接缺失，踢一脚 opencode2 run 重新拉起插件" >> "$LOG"
  timeout 120 "$OC" run "只回复：ok" --auto >> "$LOG" 2>&1
  echo "[$(date '+%F %T')] run 退出（exit=$?），等插件自己连桥" >> "$LOG"
}

misses=0
while true; do
  # 注意：`ss -tn state established '( sport = :7700 ... )'` 的过滤器语法是坏的
  # （静默返回 0，stderr 被吞），别用。按列精确匹配 7700 端口。
  conns=$(ss -tn | awk '$1 == "ESTAB" && ($4 ~ /:7700$/ || $5 ~ /:7700$/)' | wc -l)
  if [ "$conns" -lt 6 ]; then
    misses=$((misses + 1))
    # 第一次先只 run；连续两次还缺，说明 server 里的插件状态已经坏了，
    # 只 run 不会重新加载，得重启 service（2026-11-01 真机连续 4 小时没恢复）。
    if [ "$misses" -ge 2 ]; then
      boot yes
      misses=0
    else
      boot no
    fi
    sleep 15
  else
    misses=0
  fi
  sleep 60
done
