#!/usr/bin/env bash
# PR #59 集合化 V3 实战复现包的一键入口。
#
#   bash golden/v3_set_oriented_verify/run_verify.sh                 # 默认 SpinDuck 两侧
#   N=2000 bash golden/v3_set_oriented_verify/run_verify.sh           # 快速冒烟
#   WITH_REFERENCE=1 bash golden/v3_set_oriented_verify/run_verify.sh  # 追加 openGauss 存过侧
#
# 可调环境变量：
#   SPINDUCK_BIN   release CLI 路径（不设则按下面顺序探测）
#   CORPUS         被测语料工程根（默认取被测 worktree；见 README「被测版本」）
#   WORK           工作目录，默认 /tmp/v3-set-oriented-verify-work
#   N              驱动数据规模，默认 100000（冒烟建议 2000）
#   DATASETS       待核验过程，默认 "purchase byfund"
#   MODES          集合化模式序列，默认 "off batch-atomic"
#   WITH_REFERENCE 1 时执行参照侧（需 openGauss 容器，见 reference/）
#   REF_ROUNDS     参照侧计时轮数，默认 1（100K 下 byfund 一轮约 57 分钟）
#   STEP_BUDGET    call 的 --step-budget，默认 2000000000
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
WORK="${WORK:-/tmp/v3-set-oriented-verify-work}"
N="${N:-100000}"
DATASETS="${DATASETS:-purchase byfund}"
MODES="${MODES:-off batch-atomic}"
WITH_REFERENCE="${WITH_REFERENCE:-0}"
REF_ROUNDS="${REF_ROUNDS:-1}"
REF_DB_PREFIX="${REF_DB_PREFIX:-sdv3}"
CORPUS="${CORPUS:-}"
REPORTS="$HERE/reports"
mkdir -p "$WORK" "$REPORTS"

# 语料工程选择：优先本包所在树（PR #59 合并后即为主仓），回退 v3 worktree。
# 判据是「byfund 所需资产齐不齐」——缺 datasets_byfund 或包体里的
# prc_trd_hz_byfund 时不能用，否则 byfund 会在部署期报对象不存在。
pick_corpus() {
  local need_byfund=0
  case " $DATASETS " in *" byfund "*) need_byfund=1 ;; esac
  for c in "$ROOT/golden/case_corpus_purchase" \
           "$ROOT/../SpinDuck__worktrees/feat-set-oriented-v3/golden/case_corpus_purchase"; do
    [ -f "$c/sql/ddl/tables.sql" ] || continue
    if [ "$need_byfund" = 1 ]; then
      [ -f "$c/cases/datasets_byfund/seed.sql" ] || continue
      grep -rq "prc_trd_hz_byfund" "$c/sql/routines" || continue
    fi
    echo "$c"
    return 0
  done
  return 1
}
if [ -z "$CORPUS" ]; then
  CORPUS="$(pick_corpus)" || {
    echo "[verify] 未找到含 byfund 资产的语料工程；请设 CORPUS 指向被测 head 的 golden/case_corpus_purchase"
    exit 1
  }
fi

tables_of() {
  case "$1" in
    purchase) echo "dat_trd_repurchase" ;;
    byfund)   echo "dat_fund_cjqs dat_fund_xw_venture qfqscjhbrbb_temp db_log" ;;
  esac
}

# 探测 CLI：显式 > worktree release > 主仓 release
if [ -z "${SPINDUCK_BIN:-}" ]; then
  for c in "$ROOT/../SpinDuck__worktrees/feat-set-oriented-v3/spinduck/target/release/spinduck" \
           "$ROOT/spinduck/target/release/spinduck"; do
    if [ -x "$c" ]; then SPINDUCK_BIN="$c"; break; fi
  done
fi
[ -x "${SPINDUCK_BIN:-}" ] || { echo "[verify] 找不到 release CLI，请设 SPINDUCK_BIN（构建方式见 README）"; exit 1; }
export SPINDUCK_BIN CORPUS WORK STEP_BUDGET
echo "[verify] bin=$SPINDUCK_BIN"
echo "[verify] corpus=$CORPUS"
echo "[verify] $($SPINDUCK_BIN version 2>&1 | head -1)  N=$N modes='$MODES' datasets='$DATASETS'"

fail=0
for ds in $DATASETS; do
  echo
  echo "################ $ds ################"
  python3 "$HERE/sd_bench.py" "$ds" "$N" $MODES 2>&1 | tee "$REPORTS/bench_${ds}_${N}.log"
  bench_rc=${PIPESTATUS[0]}
  if [ "$bench_rc" -ne 0 ]; then echo "[FAIL] sd_bench $ds rc=$bench_rc"; fail=1; fi
  grep -q '"state_equal": true' "$REPORTS/bench_${ds}_${N}.log" \
    || { echo "[FAIL] $ds OFF/ON 全库状态不一致"; fail=1; }

  python3 "$HERE/statement_count.py" "$ds" "$N" $MODES 2>&1 \
    | tee "$REPORTS/cnt_${ds}_${N}.log"
  [ "${PIPESTATUS[0]}" -eq 0 ] || { echo "[FAIL] statement_count $ds"; fail=1; }

  if [ "$WITH_REFERENCE" = "1" ]; then
    # 多过程时忽略显式 REF_DB：同一个库里灌两套数据无法判定
    set -- $DATASETS
    ref_db="${REF_DB:-}"
    [ "$#" -gt 1 ] && ref_db=""
    ref_db="${ref_db:-${REF_DB_PREFIX}_${ds}}"
    [ -n "${REF_DB:-}" ] && [ "$#" -gt 1 ] \
      && echo "[verify] DATASETS 有多个过程，忽略 REF_DB=$REF_DB，改用 $ref_db"
    REF_DB="$ref_db" bash "$HERE/reference/sp_deploy.sh" "$ds" "$N" 2>&1 \
      | tee "$REPORTS/ref_deploy_${ds}_${N}.log"
    [ "${PIPESTATUS[0]}" -eq 0 ] || { echo "[FAIL] 参照侧部署 $ds"; fail=1; }
    bash "$HERE/reference/sp_call.sh" "$ds" "$ref_db" "$REF_ROUNDS" 2>&1 \
      | tee "$REPORTS/ref_call_${ds}_${N}.log"
    [ "${PIPESTATUS[0]}" -eq 0 ] || { echo "[FAIL] 参照侧计时 $ds"; fail=1; }
    on_mode="${MODES##* }"
    python3 "$HERE/cmp_state.py" "$WORK/db-${ds}-${N}-${on_mode}.duckdb" "$ref_db" \
      $(tables_of "$ds") 2>&1 | tee "$REPORTS/ref_cmp_${ds}_${N}.log"
    grep -q 'STATE_EQUAL' "$REPORTS/ref_cmp_${ds}_${N}.log" \
      || { echo "[FAIL] $ds 跨引擎产物不一致"; fail=1; }
  fi
done

echo
if [ "$fail" -eq 0 ]; then
  echo "VERIFY_PASS datasets='$DATASETS' N=$N modes='$MODES' reference=$WITH_REFERENCE"
else
  echo "VERIFY_FAIL 见上方 [FAIL] 行"
fi
echo "[verify] 日志：$REPORTS（bench/cnt/ref_*），工作库：$WORK"
exit "$fail"
