#!/usr/bin/env bash
# 压力测试：用大量线程 + release 优化，把"概率性并发 bug"逼近必现。
# 用法： ./scripts/stress.sh [crate] [duration_sec]
set -euo pipefail
DURATION="${2:-30}"
THREADS="${THREADS:-16}"

run_one() {
  local crate="$1"
  echo "▶ 压力测试 $crate (${DURATION}s, ${THREADS} 线程)…"
  # 反复运行，直到超时；任何一次失败立即整体失败
  timeout "$DURATION" bash -c "
    while true; do
      cargo test -p '$crate' --release --test stress -- --test-threads='$THREADS' --nocapture \
        || { echo '压力测试失败: $crate'; exit 1; }
    done
  " || true
}

if [[ "${1:-all}" == "all" ]]; then
  for c in forge-core forge-sync forge-channel forge-lockfree forge-pool forge-rt; do
    run_one "$c"
  done
else
  run_one "$1"
fi
echo "✓ 压力测试通过"
