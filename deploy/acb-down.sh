#!/usr/bin/env bash
# acb-down.sh —— 停 acb 全链路。顺序与 up 相反：驱动 → 桥 → dsh。
# 停之前先备份状态库三件套（db / -wal / -shm），少一个都可能拿到不一致的快照。
set -uo pipefail
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
STATE=$REPO/state

if [[ -f $STATE/acb.db ]]; then
  ts=$(date +%Y%m%d-%H%M%S)
  mkdir -p "$STATE/backups"
  for suffix in "" "-wal" "-shm"; do
    [[ -f $STATE/acb.db$suffix ]] && cp "$STATE/acb.db$suffix" "$STATE/backups/acb-$ts.db$suffix"
  done
  echo "已备份状态库：state/backups/acb-$ts.db*"
else
  echo "没有状态库，跳过备份"
fi

for unit in acb-drivers acb-bridge dsh-acb; do
  if systemctl --user is-active --quiet "$unit" 2>/dev/null; then
    echo "→ 停 $unit"
    systemctl --user stop "$unit"
  else
    echo "→ $unit 本来就没跑"
  fi
done

echo
echo "=== 结果（inactive 就是停干净了）==="
for unit in dsh-acb acb-bridge acb-drivers; do
  printf '%-12s %s\n' "$unit" "$(systemctl --user is-active "$unit" 2>/dev/null || true)"
done
