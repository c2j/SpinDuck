#!/usr/bin/env bash
# PR #48 集合化能力边界验证 —— 一键复现脚本。
#
# 在 PR #48 head 的临时 git worktree 中注入差分探针并运行，复现
# golden/pr48_set_oriented_verify/reports/ 下的证据。不改动主工作目录。
#
# 用法：
#   bash golden/pr48_set_oriented_verify/run_verify.sh            # 只跑正确性差分（约数秒，不含编译）
#   RUN_PERF=1 bash golden/pr48_set_oriented_verify/run_verify.sh # 追加性能基准（100k 轮约 3 分钟）
#   KEEP_WORKTREE=1 bash ...                                      # 保留临时 worktree 供检查
#   PR_HEAD=<sha> bash ...                                        # 覆盖被测 commit（默认 f62787d）
#
# 环境要求：Rust stable、cargo、git；首次构建需编译 DuckDB bundled（约 10 分钟）。
# 若主仓 spinduck/target 已有构建缓存，本脚本自动复用（CARGO_TARGET_DIR），大幅提速。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PR_HEAD="${PR_HEAD:-f62787d95fa82fb442147809978b0250ea0bb48c}"
PR_BRANCH="feat/set-oriented-v1-pr"
WT="${REPO_ROOT}-pr48-verify-$$"

cleanup() {
  if [ "${KEEP_WORKTREE:-0}" = "1" ]; then
    echo "[verify] 保留 worktree: $WT"
  else
    git -C "$REPO_ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
    echo "[verify] 已清理临时 worktree"
  fi
}
trap cleanup EXIT

echo "[verify] 被测 head: $PR_HEAD"
echo "[verify] 拉取 PR 分支 $PR_BRANCH ..."
git -C "$REPO_ROOT" fetch origin "$PR_BRANCH"

echo "[verify] 创建临时 worktree: $WT"
git -C "$REPO_ROOT" worktree add --detach "$WT" "$PR_HEAD"
git -C "$WT" submodule update --init lib/ogsql-parser

echo "[verify] 注入差分探针"
cp "$SCRIPT_DIR/probe/zz_pr48_probe.rs" "$WT/spinduck/tests/zz_pr48_probe.rs"

if [ -d "$REPO_ROOT/spinduck/target" ]; then
  export CARGO_TARGET_DIR="$REPO_ROOT/spinduck/target"
  echo "[verify] 复用构建缓存: $CARGO_TARGET_DIR"
fi

cd "$WT/spinduck"
echo "[verify] 运行正确性差分探针（边界矩阵 + 正例 + 大小写边界 + trace 计数）"
cargo test --features cli --test zz_pr48_probe -- --nocapture --skip probe_perf

if [ "${RUN_PERF:-0}" = "1" ]; then
  echo "[verify] 运行性能基准（RBAR 消除，多规模多轮）"
  cargo test --features cli --test zz_pr48_probe probe_perf -- --nocapture
fi

echo "[verify] 完成。对照 reports/correctness.log 与 reports/perf.log 核验。"