#!/usr/bin/env bash
# 集合化 V2 —— purchase 包复杂入口 prc_trd_hz_byfund N 行三方基准（SP / Off / On）。
#
# 三方：openGauss 7.0.0-RC3 原生 PACKAGE PROCEDURE CALL（存过参照）/
#       SpinDuck Off（解释器逐行）/ SpinDuck On（batch-atomic 集合化判定）。
# 同一份 N 行源数据（DuckDB generate_series → 参照侧同文件直接执行），
# 四侧输出表全列排序比对：dat_fund_cjqs / dat_fund_xw_venture /
# mid_yjqs_detail(fund_code,main_xwdm) / qfqscjhbrbb_temp。
#
# 用法：
#   bash golden/v2_set_oriented_verify-complex/run_bench.sh
#   SPINDUCK_BIN=/path/to/spinduck bash ...        # 复用已构建 CLI
#   N=1000 bash ...                                # 快速冒烟（默认 100000）
#   OG_CONTAINER=sd-og70 OG_LIBS=/opt/oglibs2 bash ...   # 启用 SP 参照方
#   ROUNDS_ON=1 bash ...                           # On 轮数
#   KEEP_WORK=1 bash ...                           # 保留临时目录
#
# 环境：Rust stable、cargo、python3；SP 参照需 docker +
#   opengauss/opengauss:7.0.0-RC3.B025（5.0.0 amd64 在 CALL 上 postmaster 自杀，V21）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CORPUS="$REPO_ROOT/golden/case_corpus_purchase"
PKG_SQL="$SCRIPT_DIR/sql"
REPORT_DIR="$SCRIPT_DIR/reports"
BIN="${SPINDUCK_BIN:-$REPO_ROOT/spinduck/target/debug/spinduck}"
N="${N:-100000}"
ROUNDS_ON="${ROUNDS_ON:-1}"
STEP_BUDGET="${STEP_BUDGET:-100000000}"
OG_CONTAINER="${OG_CONTAINER:-}"
OG_LIBS="${OG_LIBS:-/opt/oglibs2}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/v2cx_bench.XXXXXX")"
ROUTINE="pkg_split_trade_step3_sh.prc_trd_hz_byfund"
CALL_DATE="20250715"

cleanup() {
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "[v2cx] 保留临时目录: $WORK";
  else rm -rf "$WORK"; fi
}
trap cleanup EXIT

# 例程文件清单（与部署顺序一致）
routine_files() {
  local R="$CORPUS/sql/routines" S="$PKG_SQL/routines"
  for f in \
    "$R/0_SIMPLIFIED_fnc_com_getday.sql" \
    "$R/0_SIMPLIFIED_fnc_com_if_deal_date.sql" \
    "$R/2_FNC_GET_PURCHASE_JS_DAYS.sql" \
    "$S/0_STUB_fnc_com_get_fund_code.sql" \
    "$S/0_STUB_fnc_com_get_main_xwdm.sql" \
    "$S/0_STUB_fnc_com_get_seat_fee_type.sql" \
    "$S/0_STUB_fnc_com_get_seat_para.sql" \
    "$S/0_STUB_fnc_get_bond_yjlx.sql" \
    "$S/0_STUB_fnc_get_fund_data_mode.sql" \
    "$S/0_STUB_fnc_get_rate_info2.sql" \
    "$S/0_STUB_fnc_get_split_date.sql" \
    "$S/0_STUB_fnc_get_stock_kind.sql" \
    "$R/PKG_SPLIT_TRADE_STEP3_SH.sql" \
    "$CORPUS/sql/stubs/STUB_pkg_split_trade_log.sql"; do
    printf '%s\n' "$f"
  done
}

# sd_call 需要例程清单，改用文件传参避免嵌套引号
sd_call() {
  python3 - "$BIN" "$P" "$STEP_BUDGET" "$1" "$WORK/files.txt" <<'PY'
import subprocess,sys,time
bin,p,budget,mode,fl=sys.argv[1:6]
args=[bin,"call","--db",p+"/local.duckdb"]
for f in open(fl):
    f=f.strip()
    if f: args+=["--file",f]
args+=["--step-budget",budget]
if mode!="none": args+=["--set-oriented="+mode]
args+=["pkg_split_trade_step3_sh.prc_trd_hz_byfund","'20250715'","null"]
t=time.perf_counter()
r=subprocess.run(args,cwd=p,capture_output=True,text=True)
ms=(time.perf_counter()-t)*1000
sys.stdout.write(r.stdout)
print(f"ELAPSED_MS={ms:.1f}",file=sys.stderr)
if r.returncode!=0:
    sys.stderr.write(r.stderr); sys.exit(r.returncode)
PY
}

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

echo "[v2cx] CLI: $BIN ($("$BIN" version 2>/dev/null || echo '?'))"
[ -x "$BIN" ] || { echo "[v2cx] 找不到 CLI：$BIN" >&2; exit 1; }

# ---- 1. SpinDuck 工程：逐字语料 + 补充资产 ----
P="$WORK/project"; mkdir -p "$P"
cp -R "$CORPUS/sql" "$P/sql"; cp -R "$CORPUS/rules" "$P/rules"
cp "$PKG_SQL/extra_tables.sql" "$PKG_SQL/v_par_bond.sql" "$P/sql/ddl/"
cp "$PKG_SQL/routines/"0_STUB_*.sql "$P/sql/routines/"
cat > "$P/spinduck.toml" <<'EOF'
[project]
name = "v2cx"
kernel = "1.5.5"
[engine]
set_oriented = "batch-atomic"
[paths]
routines = "sql/routines"
ddl = "sql/ddl"
stubs = ["sql/stubs"]
rules = ["rules/spinduck-rules.toml"]
[[entry]]
routine = "pkg_split_trade_step3_sh.prc_trd_hz_byfund"
EOF
cat > "$P/codeweb.toml" <<'EOF'
[project]
name = "cw"
[analysis]
paths = ["sql/"]
[store]
path = ".codeweb/store.bincode"
format = "bincode"
EOF
routine_files > "$WORK/files.txt"

echo "[v2cx] compile --analyze"
"$BIN" compile --dir "$P" --analyze > "$WORK/compile.log" 2>&1 || true
GEN="$P/.spinduck/plan/$ROUTINE.toml"
[ -f "$GEN" ] || { echo "[v2cx] compile 未产出计划" >&2; tail -20 "$WORK/compile.log" >&2; exit 1; }
python3 - "$GEN" "$P/.spinduck/plan/call_plan.toml" <<'PY'
import re,sys
t=open(sys.argv[1]).read()
t=re.sub(r'\[\[missing\]\][^\[]*','',t)
# ddl 顺序：tables → 视图 → v_par_bond → extra（ALTER 依赖 par_fund_seat_para）
m=re.search(r'ddl_tables = \[(.*?)\]',t)
if m:
    files=re.findall(r'"([^"]+)"',m.group(1))
    pri={'tables.sql':0,'view_check_para.sql':1,'v_par_bond.sql':2,'extra_tables.sql':3}
    files.sort(key=lambda f: pri.get(f.rsplit('/',1)[-1],9))
    t=t[:m.start()]+'ddl_tables = ['+', '.join('"%s"'%f for f in files)+']'+t[m.end():]
open(sys.argv[2],'w').write(t)
PY
echo "[v2cx] deploy"
"$BIN" deploy "$P/.spinduck/plan/call_plan.toml" --db "$P/local.duckdb" > "$WORK/deploy.log" 2>&1 \
  || { echo "[v2cx] deploy 失败" >&2; cat "$WORK/deploy.log" >&2; exit 1; }
DB="$P/local.duckdb"

# ---- 2. 参数种子 + N 行明细（两侧同文件）----
python3 - "$PKG_SQL/seeds_params.sql" "$WORK/seeds_stmts" <<'PY'
import sys,os
sql=open(sys.argv[1]).read()
os.makedirs(sys.argv[2],exist_ok=True)
parts=[s.strip() for s in sql.split(';') if s.strip()]
for i,s in enumerate(parts):
    open(f"{sys.argv[2]}/s{i:02d}.sql",'w').write(s+';\n')
print(f"[v2cx] 参数种子语句数: {len(parts)}")
PY
for f in "$WORK"/seeds_stmts/*.sql; do
  "$BIN" query --db "$DB" --sql-file "$f" --out "$WORK/q.json" >/dev/null
done
sed "s/__N__/$N/" "$PKG_SQL/gen_detail.sql" > "$WORK/gen.sql"
"$BIN" query --db "$DB" --sql-file "$WORK/gen.sql" --out "$WORK/gen.json" >/dev/null
printf 'SELECT count(*) FROM mid_yjqs_detail;\n' > "$WORK/cnt.sql"
"$BIN" query --db "$DB" --sql-file "$WORK/cnt.sql" --out "$WORK/cnt.json" >/dev/null
SRC_N="$(python3 -c "import json;print(json.load(open('$WORK/cnt.json'))['rows'][0][0])")"
echo "[v2cx] 源行数: $SRC_N（目标 $N）"
[ "$SRC_N" = "$N" ] || { echo "[v2cx] 源行数不符" >&2; exit 1; }

# ---- 3. SpinDuck Off / On / Strict ----
printf 'DELETE FROM dat_fund_cjqs;\n' > "$WORK/tr_cjqs.sql"
printf 'DELETE FROM dat_fund_xw_venture;\n' > "$WORK/tr_vent.sql"

dump_all() {  # $1=前缀 → 四份目标表 JSON
  cat > "$WORK/d_cjqs.sql" <<'EOF'
SELECT * FROM dat_fund_cjqs ORDER BY fund_code,xwdm,security_id,bs,trade_type,bcrq,scdm,gddm,gddmzm,check_type,cjbz,stock_kind,pay_type;
EOF
  cat > "$WORK/d_vent.sql" <<'EOF'
SELECT * FROM dat_fund_xw_venture ORDER BY fund_code,xwdm,trade_type,venture_type,scdm,operation_kind,bcrq;
EOF
  cat > "$WORK/d_mid.sql" <<'EOF'
SELECT cjbh,fund_code,main_xwdm FROM mid_yjqs_detail ORDER BY cjbh;
EOF
  cat > "$WORK/d_qf.sql" <<'EOF'
SELECT * FROM qfqscjhbrbb_temp ORDER BY gddm,gddmzm,gsdm,bcrq,bs,zqdm,seq_no;
EOF
  for t in cjqs vent mid qf; do
    "$BIN" query --db "$DB" --sql-file "$WORK/d_$t.sql" --out "$WORK/${1}_$t.json" >/dev/null
  done
}

echo "[v2cx] SpinDuck Off pass"
sd_call off > "$WORK/off_call.txt" 2> "$WORK/off_ms.txt"
OFF_MS="$(sed -n 's/ELAPSED_MS=//p' "$WORK/off_ms.txt")"
dump_all off

echo "[v2cx] SpinDuck On (batch-atomic) × $ROUNDS_ON"
declare -a ON_MS=()
for r in $(seq 1 "$ROUNDS_ON"); do
  "$BIN" query --db "$DB" --sql-file "$WORK/tr_cjqs.sql" --out "$WORK/t.json" >/dev/null
  "$BIN" query --db "$DB" --sql-file "$WORK/tr_vent.sql" --out "$WORK/t.json" >/dev/null
  sd_call batch-atomic > "$WORK/on_call_r${r}.txt" 2> "$WORK/on_ms_r${r}.txt"
  ON_MS+=("$(sed -n 's/ELAPSED_MS=//p' "$WORK/on_ms_r${r}.txt")")
  echo "[v2cx]   on round $r: ${ON_MS[$((r-1))]}ms"
done
ON_MED="$(median "${ON_MS[@]}")"
"$BIN" query --db "$DB" --sql-file "$WORK/tr_cjqs.sql" --out "$WORK/t.json" >/dev/null
"$BIN" query --db "$DB" --sql-file "$WORK/tr_vent.sql" --out "$WORK/t.json" >/dev/null
sd_call batch-atomic > "$WORK/on_call.txt" 2>/dev/null
dump_all on

echo "[v2cx] SpinDuck Strict pass"
"$BIN" query --db "$DB" --sql-file "$WORK/tr_cjqs.sql" --out "$WORK/t.json" >/dev/null
"$BIN" query --db "$DB" --sql-file "$WORK/tr_vent.sql" --out "$WORK/t.json" >/dev/null
sd_call strict > "$WORK/strict_call.txt" 2> "$WORK/strict_ms.txt"
STRICT_MS="$(sed -n 's/ELAPSED_MS=//p' "$WORK/strict_ms.txt")"
dump_all strict

# ---- 4. SP 参照方（可选）----
SP_MS=""; REF_N=""
if [ -n "$OG_CONTAINER" ]; then
  echo "[v2cx] SP 参照：$OG_CONTAINER 部署 + 载入 + CALL"
  docker exec "$OG_CONTAINER" mkdir -p /tmp/hz
  # DDL（顺序敏感：tables → views → v_par_bond → extra）
  docker cp "$CORPUS/sql/ddl/tables.sql" "$OG_CONTAINER:/tmp/hz/tables.sql"
  docker cp "$CORPUS/sql/ddl/views/view_check_para.sql" "$OG_CONTAINER:/tmp/hz/view_check_para.sql"
  docker cp "$PKG_SQL/v_par_bond.sql" "$PKG_SQL/extra_tables.sql" "$OG_CONTAINER:/tmp/hz/"
  docker cp "$CORPUS/sql/routines/PKG_SPLIT_TRADE_STEP3_SH.sql" "$OG_CONTAINER:/tmp/hz/"
  docker cp "$CORPUS/sql/stubs/STUB_pkg_split_trade_log.sql" "$OG_CONTAINER:/tmp/hz/"
  for f in "$PKG_SQL"/routines/0_STUB_*.sql "$CORPUS"/sql/routines/0_SIMPLIFIED_*.sql "$CORPUS"/sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql; do
    docker cp "$f" "$OG_CONTAINER:/tmp/hz/$(basename "$f")"
  done
  cat > "$WORK/ref_run.sh" <<EOF
#!/bin/bash
export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$OG_LIBS
exec /usr/local/opengauss/bin/gsql -U gaussdb -W "\$GS_PASSWORD" -d hz -p 5432 -v ON_ERROR_STOP=1 "\$@"
EOF
  docker cp "$WORK/ref_run.sh" "$OG_CONTAINER:/tmp/hz/run.sh"
  docker exec "$OG_CONTAINER" bash -lc 'chmod +x /tmp/hz/run.sh; for f in tables.sql view_check_para.sql v_par_bond.sql extra_tables.sql 0_SIMPLIFIED_fnc_com_getday.sql 0_SIMPLIFIED_fnc_com_if_deal_date.sql 2_FNC_GET_PURCHASE_JS_DAYS.sql 0_STUB_fnc_com_get_fund_code.sql 0_STUB_fnc_com_get_main_xwdm.sql 0_STUB_fnc_com_get_seat_fee_type.sql 0_STUB_fnc_com_get_seat_para.sql 0_STUB_fnc_get_bond_yjlx.sql 0_STUB_fnc_get_fund_data_mode.sql 0_STUB_fnc_get_rate_info2.sql 0_STUB_fnc_get_split_date.sql 0_STUB_fnc_get_stock_kind.sql STUB_pkg_split_trade_log.sql PKG_SPLIT_TRADE_STEP3_SH.sql; do /tmp/hz/run.sh -f /tmp/hz/$f >/dev/null || { echo "deploy fail $f" >&2; exit 1; }; done' \
    || { echo "[v2cx] 参照侧例程部署失败" >&2; exit 1; }
  # 参数种子：单语句文件逐条执行
  for f in "$WORK"/seeds_stmts/*.sql; do
    docker cp "$f" "$OG_CONTAINER:/tmp/hz/seed_one.sql"
    docker exec "$OG_CONTAINER" bash -lc '/tmp/hz/run.sh -f /tmp/hz/seed_one.sql >/dev/null' || { echo "[v2cx] 参照种子失败 $f" >&2; exit 1; }
  done
  sed "s/__N__/$N/" "$PKG_SQL/gen_detail.sql" > "$WORK/hz_gen.sql"
  docker cp "$WORK/hz_gen.sql" "$OG_CONTAINER:/tmp/hz/hz_gen.sql"
  docker exec "$OG_CONTAINER" bash -lc '/tmp/hz/run.sh -f /tmp/hz/hz_gen.sql >/dev/null'
  REF_N="$(docker exec "$OG_CONTAINER" bash -lc "/tmp/hz/run.sh -tA -c 'SELECT count(*) FROM mid_yjqs_detail'" | tr -d '[:space:]')"
  echo "[v2cx] 参照源行数: $REF_N"
  [ "$REF_N" = "$N" ] || { echo "[v2cx] 参照源行数不符" >&2; exit 1; }
  printf "CALL %s('%s', NULL);\n" "$ROUTINE" "$CALL_DATE" > "$WORK/call_ref.sql"
  docker cp "$WORK/call_ref.sql" "$OG_CONTAINER:/tmp/hz/call_ref.sql"
  SP_MS="$(python3 - "$OG_CONTAINER" <<'PY'
import subprocess,sys,time
c=sys.argv[1]
t=time.perf_counter()
r=subprocess.run(["docker","exec",c,"bash","-lc","/tmp/hz/run.sh -f /tmp/hz/call_ref.sql"],capture_output=True,text=True)
ms=(time.perf_counter()-t)*1000
sys.stderr.write(r.stderr[-300:])
if r.returncode!=0:
    sys.stderr.write(r.stdout[-800:]); sys.exit(r.returncode)
print(f"{ms:.1f}")
PY
)"
  echo "[v2cx] SP CALL: ${SP_MS}ms"
  # 参照导出（CSV → 归一化比对）
  cat > "$WORK/export_ref.sql" <<'EOF'
\copy (SELECT * FROM dat_fund_cjqs ORDER BY fund_code,xwdm,security_id,bs,trade_type,bcrq,scdm,gddm,gddmzm,check_type,cjbz,stock_kind,pay_type) TO '/tmp/hz/ref_cjqs.csv' WITH (FORMAT csv, DELIMITER ',');
\copy (SELECT * FROM dat_fund_xw_venture ORDER BY fund_code,xwdm,trade_type,venture_type,scdm,operation_kind,bcrq) TO '/tmp/hz/ref_vent.csv' WITH (FORMAT csv, DELIMITER ',');
\copy (SELECT cjbh,fund_code,main_xwdm FROM mid_yjqs_detail ORDER BY cjbh) TO '/tmp/hz/ref_mid.csv' WITH (FORMAT csv, DELIMITER ',');
\copy (SELECT * FROM qfqscjhbrbb_temp ORDER BY gddm,gddmzm,gsdm,bcrq,bs,zqdm,seq_no) TO '/tmp/hz/ref_qf.csv' WITH (FORMAT csv, DELIMITER ',');
EOF
  docker cp "$WORK/export_ref.sql" "$OG_CONTAINER:/tmp/hz/export_ref.sql"
  docker exec "$OG_CONTAINER" bash -lc '/tmp/hz/run.sh -f /tmp/hz/export_ref.sql >/dev/null'
  for t in cjqs vent mid qf; do
    docker cp "$OG_CONTAINER:/tmp/hz/ref_$t.csv" "$WORK/ref_$t.csv" 2>/dev/null || true
  done
fi

# ---- 5. 一致性断言 ----
mkdir -p "$REPORT_DIR"
python3 - "$WORK" "$N" > "$WORK/consistency.txt" 2>&1 <<'PY' || true
import json,csv,sys,os
w=sys.argv[1]; n=int(sys.argv[2])
def norm_json(p):
    d=json.load(open(p)); return sorted(tuple('' if v is None else str(v) for v in r) for r in d['rows'])
def norm_csv(p):
    if not os.path.exists(p): return None
    return sorted(tuple('' if v is None else str(v) for v in r) for r in csv.reader(open(p)))
ok=True
for t in ['cjqs','vent','mid','qf']:
    off=norm_json(f'{w}/off_{t}.json'); on=norm_json(f'{w}/on_{t}.json'); st=norm_json(f'{w}/strict_{t}.json')
    ref=norm_csv(f'{w}/ref_{t}.csv')
    line=f"{t}: rows off={len(off)} on={len(on)} strict={len(st)}" + (f" ref={len(ref)}" if ref is not None else " ref=<无>")
    eq = off==on==st
    line+=f"  off==on==strict:{eq}"
    if ref is not None:
        line+=f"  off==ref:{off==ref} on==ref:{on==ref}"
        eq = eq and off==ref
    print(line); ok = ok and eq
print("CONSISTENT=" + ("OK" if ok else "FAIL"))
PY
cat "$WORK/consistency.txt"
grep -q "CONSISTENT=OK" "$WORK/consistency.txt" || { echo "[v2cx] 一致性断言失败" >&2; exit 1; }

# ---- 6. 汇总 ----
SPEEDUP_OFF_ON=$(python3 -c "print(f'{$OFF_MS/$ON_MED:.1f}')" 2>/dev/null || echo n/a)
if [ -n "$SP_MS" ]; then SPEEDUP_SP_ON=$(python3 -c "print(f'{$SP_MS/$ON_MED:.1f}')"); else SPEEDUP_SP_ON="n/a"; fi
{
  echo "=== 集合化 V2 purchase 复杂入口 ${N} 三方基准 ==="
  echo "CLI: $BIN  构建: debug  step-budget: $STEP_BUDGET"
  echo "过程: $ROUTINE（真实语料逐字 + 补充桩/DDL，见 sql/）"
  echo "参照: openGauss 7.0.0-RC3 ${OG_CONTAINER:-<未启用>}  库: hz"
  echo "数据: mid_yjqs_detail $N 行同源（generate_series 互质乘子打散分组键），源行数 SpinDuck=$SRC_N 参照=${REF_N:-n/a}"
  echo "分组数（c_get_data 预估）: ~90k（zqdm '60'||g 唯一化）"
  echo ""
  echo "方 | 墙钟(ms) | 各轮(ms) | 口径"
  echo "SP 存过（openGauss 原生 CALL） | ${SP_MS:-n/a} | - | gsql 客户端墙钟"
  echo "SpinDuck Off（解释器逐行） | $OFF_MS | $OFF_MS | CLI call 端到端"
  echo "SpinDuck On（batch-atomic） | $ON_MED | ${ON_MS[*]} | CLI call 端到端"
  echo "SpinDuck Strict | $STRICT_MS | - | CLI call 端到端"
  echo ""
  echo "Off→On: ${SPEEDUP_OFF_ON}×；SP→On: ${SPEEDUP_SP_ON}×"
  echo ""
  echo "集合化判定:"
  grep '^SET_ORIENTED' "$WORK/on_call.txt" 2>/dev/null | sed 's/^/  On: /' || echo "  On: <无报告行>"
  grep '^SET_ORIENTED' "$WORK/off_call.txt" 2>/dev/null | sed 's/^/  Off: /' || echo "  Off: <无报告行（feature 关）>"
  grep '^SET_ORIENTED' "$WORK/strict_call.txt" 2>/dev/null | sed 's/^/  Strict: /' || echo "  Strict: <无报告行>"
  echo ""
  echo "数据一致性（四表全列排序逐行）:"
  sed 's/^/  /' "$WORK/consistency.txt"
  echo ""
  echo "口径: debug 构建；SpinDuck=CLI 端到端墙钟，SP=gsql 客户端墙钟（含连接）。"
} > "$REPORT_DIR/bench_summary.log"
cp "$WORK/on_call.txt" "$REPORT_DIR/v2_on_call.txt" 2>/dev/null || true
cp "$WORK/off_call.txt" "$REPORT_DIR/v2_off_call.txt"
cp "$WORK/strict_call.txt" "$REPORT_DIR/v2_strict_call.txt" 2>/dev/null || true
for t in cjqs vent mid qf; do
  [ -f "$WORK/ref_$t.csv" ] && cp "$WORK/ref_$t.csv" "$REPORT_DIR/ref_$t.csv"
done
cat "$REPORT_DIR/bench_summary.log"
echo "[v2cx] 完成。报告: $REPORT_DIR/bench_summary.log"
