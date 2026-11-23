#!/usr/bin/env bash
# openGauss 参照侧存过计时：在 postmaster 内用 clock_timestamp() 夹住过程调用，
# 记录服务端 ELAPSED_MS，同时打印客户端整段墙钟与产物表行数。
#
# 用法:  bash reference/sp_call.sh <purchase|byfund> [REF_DB] [ROUNDS]
#        ROUNDS>1 时先 TRUNCATE 一次，逐轮计时（过程开头会自行清表重写，可重入）。
#
# 崩溃口径：按 golden/README 的取证方式，调用前后对比容器
# RestartCount / StartedAt / ExitCode；V21 记录的 procedure CALL 崩溃
# 在参照实例（openGauss 7.0.0-RC3）上不复现，若复现则本脚本会显示 RestartCount 增加。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTAINER="${CONTAINER:-sd-og70}"
DATASET="${1:?用法: sp_call.sh <purchase|byfund> [REF_DB] [ROUNDS]}"
REF_DB="${2:-sdv3_${DATASET}}"
ROUNDS="${3:-1}"
DATE="${DATE:-20250715}"
OUT_DIR="${OUT_DIR:-${TMPDIR:-/tmp}/v3-set-oriented-verify-ref}"
mkdir -p "$OUT_DIR"

case "$DATASET" in
  purchase) ROUTINE=prc_trd_deal_purchase; TARGETS="dat_trd_repurchase" ;;
  byfund)   ROUTINE=prc_trd_hz_byfund;     TARGETS="dat_fund_cjqs dat_fund_xw_venture qfqscjhbrbb_temp" ;;
  *) echo "[sp] 未知 dataset: $DATASET" >&2; exit 2 ;;
esac

gsql() {
  docker exec -i "$CONTAINER" bash -lc \
    'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$LD_LIBRARY_PATH
     /usr/local/opengauss/bin/gsql -U gaussdb -W "$GS_PASSWORD" -d "$1" -p 5432 \
       -v ON_ERROR_STOP=1 -tA' sh "$1"
}

state() { docker inspect -f '{{.State.Status}} restarts={{.RestartCount}} started={{.State.StartedAt}} exit={{.State.ExitCode}}' "$CONTAINER"; }

echo "[sp] $DATASET -> $REF_DB .$ROUTINE rounds=$ROUNDS"
echo "[sp] before: $(state)"

sed -e "s/:ROUTINE/${ROUTINE}/" -e "s/:DATE/${DATE}/" "$HERE/reference/sp_call_body.sql" \
  > "$OUT_DIR/sp_call_${DATASET}.sql"

for i in $(seq 1 "$ROUNDS"); do
  if [ "$i" -gt 1 ]; then
    # 轮间清空产物表，让每轮从同一可观察状态起步（与 SpinDuck 侧每轮独立 deploy 对齐）
    printf 'TRUNCATE %s;\n' "$TARGETS" | gsql "$REF_DB" > "$OUT_DIR/truncate.log" 2>&1 \
      || { echo "[sp] TRUNCATE 失败"; tail -3 "$OUT_DIR/truncate.log"; exit 1; }
  fi
  s0=$(date +%s.%N)
  gsql "$REF_DB" < "$OUT_DIR/sp_call_${DATASET}.sql" > "$OUT_DIR/sp_call_${DATASET}_r${i}.out" 2>&1
  rc=$?
  s1=$(date +%s.%N)
  printf '[sp] round %s rc=%s client_wall=%ss server_notice=%s\n' \
    "$i" "$rc" \
    "$(python3 -c "print(f'{$s1-$s0:.3f}')")" \
    "$(grep -o 'ELAPSED_MS=[0-9.]* out=[^ ]*' "$OUT_DIR/sp_call_${DATASET}_r${i}.out" | head -1)"
  [ "$rc" -eq 0 ] || { tail -5 "$OUT_DIR/sp_call_${DATASET}_r${i}.out"; break; }
done

echo "[sp] after:  $(state)"
for t in $TARGETS; do
  printf '[sp] rows %-22s %s\n' "$t" "$(gsql "$REF_DB" <<< "SELECT count(*) FROM $t;" 2>/dev/null)"
done
