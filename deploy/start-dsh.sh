#!/usr/bin/env bash
# start-dsh.sh —— dsh-acb.service 的入口。
#
# 单元文件里不写死 nix store 路径：dsh 升级后路径会变。这里按 acb-up.sh 原来的
# 逻辑解析 dsh 包装器旁边的 dsh-real，再 exec 过去。
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
PATCH=$REPO/harnesses/dsh/dsh-mcp.patch.yml
ENV_FILE=$HERE/acb.env

fail() { echo "✗ $*" >&2; exit 1; }

[[ -f $PATCH ]] || fail "没有 $PATCH"

DSH_BIN=${DSH_BIN:-}
if [[ -z $DSH_BIN ]]; then
  dsh_wrapper=$(command -v dsh || true)
  [[ -n $dsh_wrapper ]] || fail "PATH 里没有 dsh，没法定位 dsh-real"
  DSH_BIN="$(dirname "$(readlink -f "$dsh_wrapper")")/dsh-real"
fi
[[ -x $DSH_BIN ]] || fail "找不到可执行的 dsh-real：$DSH_BIN"

if [[ -f $ENV_FILE ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ENV_FILE"
  set +a
else
  echo "⚠ 没有 $ENV_FILE，dsh 会因缺 API key 报 NO_ADAPTER（见 deploy/acb.env.example）" >&2
fi

exec "$DSH_BIN" --profile nix-web-acb --patch "$PATCH" --no-open --port 3080
