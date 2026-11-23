#!/usr/bin/env bash
# V4 loop-parallel —— purchase / byfund 真实语料验证脚本。
#
# 在当前 worktree（须含 loop-parallel 实现）上运行差分探针：
#   1. 探针源码注入 spinduck/tests/（autodiscovery 自动注册）；
#   2. cargo test --features native 跑 6 个用例（等价 ×4 + 性能 ×2）；
#   3. 全部 V4| 行与测试结果落 reports/probe_run.log。
#
# 用法（仓库根目录）：
#   bash golden/v4_loop_parallel_verify/run_verify.sh
#   KEEP_PROBE=1 bash ...   # 保留注入的探针文件
#
# 环境要求：Rust stable、cargo；native feature 需本机 DuckDB bundled 编译
# （首次约 10 分钟，target 缓存后秒级）。无需 GaussDB 容器。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SPINDUCK_DIR="$REPO_ROOT/spinduck"
PROBE_SRC="$SCRIPT_DIR/probe/zz_v4_loop_parallel_probe.rs"
PROBE_DST="$SPINDUCK_DIR/tests/zz_v4_loop_parallel_probe.rs"
REPORT_DIR="$SCRIPT_DIR/reports"
mkdir -p "$REPORT_DIR"

cp "$PROBE_SRC" "$PROBE_DST"
cleanup() {
  if [ "${KEEP_PROBE:-0}" != "1" ]; then
    rm -f "$PROBE_DST"
  fi
}
trap cleanup EXIT

cd "$SPINDUCK_DIR"
cargo test --features native --test zz_v4_loop_parallel_probe -- --nocapture \
  2>&1 | tee "$REPORT_DIR/probe_run.log"

echo "[v4] 报告已写入 $REPORT_DIR/probe_run.log"
