#!/usr/bin/env bash
# PR #48 集合化 —— purchase 同形可转换过程 100k 三方基准。
#
# 三方：openGauss 原生 plpgsql 游标（存过）/ SpinDuck OFF（解释器游标 RBAR）/
#       SpinDuck ON（集合化 INSERT...SELECT）。同一份 100k 源数据（CSV 同源），
#       各 3 轮测时取中位数，目标表排序后三方逐行比对。
#
# 用法：
#   bash golden/pr48_set_oriented_verify/bench/run_bench.sh
#   SPINDUCK_BIN=/path/to/spinduck bash ...   # 复用已构建 PR-head CLI
#   ROUNDS=5 bash ...                         # 覆盖轮数（默认 3）
#   KEEP_WORKTREE=1 bash ...                  # 保留临时目录
#
# 环境：Rust stable、cargo、git、python3、docker（spinduck-gaussdb 容器须 healthy）。
# 口令经容器 env 的 $GS_PASSWORD 引用在容器内展开，不上命令行/输出。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERIFY_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$VERIFY_DIR/../.." && pwd)"
REPORT_DIR="$SCRIPT_DIR/reports"
PR_HEAD="${PR_HEAD:-f62787d95fa82fb442147809978b0250ea0bb48c}"
PR_BRANCH="feat/set-oriented-v1-pr"
ROUNDS="${ROUNDS:-3}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr48_bench.XXXXXX")"
BIN="${SPINDUCK_BIN:-}"
WT=""
CONTAINER="${SPINDUCK_GAUSS_CONTAINER:-spinduck-gaussdb}"
ORDER="gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,cjsl"

cleanup() {
  if [ "${KEEP_WORKTREE:-0}" = "1" ]; then echo "[bench] 保留临时目录: $WORK";
  else
    [ -n "$WT" ] && git -C "$REPO_ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

# ---- gsql 包装：口令留容器 env，SQL 经 stdin ----
gsql_stdin() {  # $1=sql 文件
  docker exec -i "$CONTAINER" bash -lc \
    'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$LD_LIBRARY_PATH; /usr/local/opengauss/bin/gsql -U gaussdb -W "$GS_PASSWORD" -d postgres -p 5432 -v ON_ERROR_STOP=1 -tA' < "$1"
}

# ---- 1. PR-head CLI ----
if [ -z "$BIN" ]; then
  echo "[bench] 构建 PR head CLI ($PR_HEAD)"
  git -C "$REPO_ROOT" fetch origin "$PR_BRANCH"
  WT="${REPO_ROOT}-pr48-bench-$$"
  git -C "$REPO_ROOT" worktree add --detach "$WT" "$PR_HEAD"
  git -C "$WT" submodule update --init lib/ogsql-parser
  [ -d "$REPO_ROOT/spinduck/target" ] && export CARGO_TARGET_DIR="$REPO_ROOT/spinduck/target"
  ( cd "$WT/spinduck" && cargo build --features cli --bin spinduck )
  BIN="${CARGO_TARGET_DIR:-$WT/spinduck/target}/debug/spinduck"
fi
echo "[bench] CLI: $BIN ($("$BIN" version))"

# ---- 2. 容器探活 ----
docker exec "$CONTAINER" bash -lc 'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$LD_LIBRARY_PATH; /usr/local/opengauss/bin/gsql -U gaussdb -W "$GS_PASSWORD" -d postgres -p 5432 -tAc "SELECT 1"' >/dev/null 2>&1 \
  || { echo "[bench] 参照容器 $CONTAINER 不可连（先 docker compose -f golden/gaussdb-docker/docker-compose.yml up -d）" >&2; exit 1; }

# ---- 3. 工作目录 + 资产 ----
cp "$SCRIPT_DIR"/sql/*.sql "$WORK/"
cat > "$WORK/spinduck.toml" <<'EOF'
[project]
name = "pr48_bench"
kernel = "1.5.5"
[paths]
routines = "."
home = ".spinduck"
[engine]
set_oriented = true
EOF
cat > "$WORK/timecmd.py" <<'PY'
import subprocess, sys, time
t0 = time.perf_counter()
r = subprocess.run(sys.argv[1:])
print(f"ELAPSED_MS={(time.perf_counter()-t0)*1000:.3f}", file=sys.stderr)
sys.exit(r.returncode)
PY
printf 'TRUNCATE dat_fund_cjqs_so;' > "$WORK/trunc.sql"
cat > "$WORK/noop.sql" <<'EOF'
CREATE OR REPLACE PROCEDURE prc_noop IS BEGIN NULL; END;
/
EOF
sd_call() {  # $1=procedure 文件 $2=例程名 → 测时，stdout 业务行，stderr ELAPSED
  python3 "$WORK/timecmd.py" "$BIN" call --db "$WORK/local.duckdb" --file "$1" "$2"
}
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# ---- 4. SpinDuck 建表 + 生成 100k + 导出 CSV ----
echo "[bench] SpinDuck 建表 + 生成 100k"
# query 一次一条：拆分 DDL 两条 CREATE
awk 'BEGIN{n=0} /CREATE TABLE/{n++; f=sprintf("'"$WORK"'/ddl_%d.sql",n)} {if(n>0) print > f}' "$WORK/spinduck_tables.sql"
for f in "$WORK"/ddl_*.sql; do "$BIN" query --db "$WORK/local.duckdb" --sql-file "$f" --out "$WORK/j.json" >/dev/null; done
"$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/gen_data.sql" --out "$WORK/j.json" >/dev/null
printf "COPY mid_yjqs_detail_so TO '%s/src.csv' (DELIMITER ',');" "$WORK" > "$WORK/export_src.sql"
"$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/export_src.sql" --out "$WORK/j.json" >/dev/null

# ---- 5. 参照侧建表/函数 + 同源载入 ----
echo "[bench] 参照侧部署 + CSV 同源载入"
gsql_stdin "$WORK/reference_setup.sql" >/dev/null
docker cp "$WORK/src.csv" "$CONTAINER:/tmp/src.csv"
docker exec "$CONTAINER" chmod 644 /tmp/src.csv
cat > "$WORK/ref_load.sql" <<'EOF'
\copy mid_yjqs_detail_so FROM '/tmp/src.csv' WITH (FORMAT csv, HEADER true, DELIMITER ',')
EOF
gsql_stdin "$WORK/ref_load.sql" >/dev/null
printf 'SELECT count(*) FROM mid_yjqs_detail_so;' > "$WORK/count_src.sql"
"$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/count_src.sql" --out "$WORK/count_src.json" >/dev/null
SD_SRC=$(python3 -c "import json;print(json.load(open('$WORK/count_src.json'))['rows'][0][0])")
REF_SRC=$(gsql_stdin "$WORK/count_src.sql" | tr -d '[:space:]')
echo "[bench] 源数据行数 SpinDuck=$SD_SRC Reference=$REF_SRC"
[ "$SD_SRC" = "100000" ] && [ "$REF_SRC" = "100000" ] || { echo "[bench] 源数据非 100k 或不一致" >&2; exit 1; }

# ---- 6. 三方各 ROUNDS 轮测时 ----
cat > "$WORK/ref_time.sql" <<'EOF'
TRUNCATE dat_fund_cjqs_so;
DO $$
DECLARE t0 timestamptz; t1 timestamptz; n bigint;
BEGIN
  t0 := clock_timestamp(); n := fnc_copy_purchase_so(); t1 := clock_timestamp();
  RAISE NOTICE 'ELAPSED_MS=% copied=%', round(extract(epoch from (t1-t0))*1000,3), n;
END $$;
EOF
declare -a ON_MS OFF_MS REF_MS
echo "[bench] 测时（每方 $ROUNDS 轮）"
for r in $(seq 1 "$ROUNDS"); do
  # ON
  sed -i.bak 's/set_oriented = .*/set_oriented = true/' "$WORK/spinduck.toml"
  "$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/trunc.sql" --out "$WORK/j.json" >/dev/null
  on=$(sd_call "$WORK/copy_procedure.sql" prc_copy_purchase_so 2>&1 >/dev/null | sed -n 's/ELAPSED_MS=//p')
  ON_MS+=("$on")
  # OFF
  sed -i.bak 's/set_oriented = .*/set_oriented = false/' "$WORK/spinduck.toml"
  "$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/trunc.sql" --out "$WORK/j.json" >/dev/null
  off=$(sd_call "$WORK/copy_procedure.sql" prc_copy_purchase_so 2>&1 >/dev/null | sed -n 's/ELAPSED_MS=//p')
  OFF_MS+=("$off")
  # Reference
  ref=$(gsql_stdin "$WORK/ref_time.sql" 2>&1 | sed -n 's/.*ELAPSED_MS=\([0-9.]*\).*/\1/p')
  REF_MS+=("$ref")
  echo "[bench]   round $r: ON=${on}ms OFF=${off}ms REF=${ref}ms"
done
ON_MED=$(median "${ON_MS[@]}"); OFF_MED=$(median "${OFF_MS[@]}"); REF_MED=$(median "${REF_MS[@]}")

# ---- 7. 固定开销（noop call，3 轮）----
declare -a NOOP_MS
sed -i.bak 's/set_oriented = .*/set_oriented = true/' "$WORK/spinduck.toml"
for r in 1 2 3; do
  NOOP_MS+=("$(sd_call "$WORK/noop.sql" prc_noop 2>&1 >/dev/null | sed -n 's/ELAPSED_MS=//p')")
done
NOOP_MED=$(median "${NOOP_MS[@]}")

# ---- 8. 三方一致性：清洁调用后导出排序目标，归一表头，逐行 diff ----
echo "[bench] 三方目标表一致性比对"
norm() { if head -1 "$1" | grep -q '^gddm,'; then tail -n +2 "$1"; else cat "$1"; fi; }
# ON 目标
sed -i.bak 's/set_oriented = .*/set_oriented = true/' "$WORK/spinduck.toml"
"$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/trunc.sql" --out "$WORK/j.json" >/dev/null
"$BIN" call --db "$WORK/local.duckdb" --file "$WORK/copy_procedure.sql" prc_copy_purchase_so > "$WORK/on_call.txt" 2>&1
printf "COPY (SELECT * FROM dat_fund_cjqs_so ORDER BY %s) TO '%s/tgt_on.csv' (DELIMITER ',');" "$ORDER" "$WORK" > "$WORK/e.sql"
"$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/e.sql" --out "$WORK/j.json" >/dev/null
# OFF 目标
sed -i.bak 's/set_oriented = .*/set_oriented = false/' "$WORK/spinduck.toml"
"$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/trunc.sql" --out "$WORK/j.json" >/dev/null
"$BIN" call --db "$WORK/local.duckdb" --file "$WORK/copy_procedure.sql" prc_copy_purchase_so > "$WORK/off_call.txt" 2>&1
printf "COPY (SELECT * FROM dat_fund_cjqs_so ORDER BY %s) TO '%s/tgt_off.csv' (DELIMITER ',');" "$ORDER" "$WORK" > "$WORK/e.sql"
"$BIN" query --db "$WORK/local.duckdb" --sql-file "$WORK/e.sql" --out "$WORK/j.json" >/dev/null
# Reference 目标
gsql_stdin "$WORK/ref_time.sql" >/dev/null 2>&1
printf "\\copy (SELECT * FROM dat_fund_cjqs_so ORDER BY %s) TO '/tmp/tgt_ref.csv' WITH (FORMAT csv, DELIMITER ',')" "$ORDER" > "$WORK/ref_export.sql"
gsql_stdin "$WORK/ref_export.sql" >/dev/null
docker cp "$CONTAINER:/tmp/tgt_ref.csv" "$WORK/tgt_ref.csv"
norm "$WORK/tgt_on.csv" > "$WORK/on.norm"
norm "$WORK/tgt_off.csv" > "$WORK/off.norm"
norm "$WORK/tgt_ref.csv" > "$WORK/ref.norm"
CONSISTENT=OK
diff "$WORK/ref.norm" "$WORK/on.norm" > "$WORK/diff_on.txt" || CONSISTENT=FAIL
diff "$WORK/ref.norm" "$WORK/off.norm" > "$WORK/diff_off.txt" || CONSISTENT=FAIL
ON_ROWS=$(wc -l < "$WORK/on.norm" | tr -d ' '); OFF_ROWS=$(wc -l < "$WORK/off.norm" | tr -d ' '); REF_ROWS=$(wc -l < "$WORK/ref.norm" | tr -d ' ')

# ---- 9. 断言 + 报告 ----
grep -q 'SET_ORIENTED prc_copy_purchase_so converted' "$WORK/on_call.txt" || { echo "[bench] ON 未转换" >&2; exit 1; }
grep -q 'SET_ORIENTED' "$WORK/off_call.txt" && { echo "[bench] OFF 不应有集合化报告" >&2; exit 1; }
[ "$CONSISTENT" = OK ] || { echo "[bench] 三方一致性失败" >&2; cat "$WORK/diff_on.txt" "$WORK/diff_off.txt" >&2; exit 1; }
[ "$ON_ROWS" = 100000 ] && [ "$OFF_ROWS" = 100000 ] && [ "$REF_ROWS" = 100000 ] || { echo "[bench] 目标行数非 100k" >&2; exit 1; }

mkdir -p "$REPORT_DIR"
SPEEDUP=$(python3 -c "print(f'{$OFF_MED/$ON_MED:.1f}')")
{
  echo "=== PR#48 purchase 同形可转换过程 100k 三方基准 ==="
  echo "CLI: $("$BIN" version)  head: $PR_HEAD  轮数: $ROUNDS  构建: debug"
  echo "数据: 100000 行同源（CSV 转移），源行数 SpinDuck=$SD_SRC Reference=$REF_SRC"
  echo ""
  echo "方 | 中位墙钟(ms) | 各轮(ms) | 口径"
  echo "SpinDuck OFF（解释器游标 RBAR） | $OFF_MED | ${OFF_MS[*]} | CLI call 端到端"
  echo "SpinDuck ON（集合化 INSERT...SELECT） | $ON_MED | ${ON_MS[*]} | CLI call 端到端"
  echo "openGauss 原生（plpgsql 游标 RBAR） | $REF_MED | ${REF_MS[*]} | 纯函数执行(clock_timestamp)"
  echo ""
  echo "SpinDuck 固定开销（noop call 中位）: $NOOP_MED ms（进程启动+开 100k 库+注册+事务）"
  echo "OFF→ON 加速比: ${SPEEDUP}×（同为 CLI 端到端口径）"
  echo ""
  echo "数据一致性: 三方目标表排序后逐行比对 = $CONSISTENT"
  echo "目标行数: ON=$ON_ROWS OFF=$OFF_ROWS REF=$REF_ROWS（均 100000）"
  echo "ON 判定: $(grep SET_ORIENTED "$WORK/on_call.txt")"
  echo ""
  echo "口径说明: SpinDuck 为 CLI 端到端墙钟（含固定开销 $NOOP_MED ms）；参照为纯函数执行。"
  echo "ON 墙钟主要由固定开销+落盘提交构成，净集合化执行远小于墙钟（进程内探针 100k 约 50ms 佐证）。"
  echo "debug 构建绝对值仅证明方向与量级，非 release 承诺。"
} > "$REPORT_DIR/bench_summary.log"
cp "$WORK/on_call.txt" "$REPORT_DIR/bench_on_call.txt"
cp "$WORK/off_call.txt" "$REPORT_DIR/bench_off_call.txt"
cat "$REPORT_DIR/bench_summary.log"
echo "[bench] 通过。报告: $REPORT_DIR/bench_summary.log"