#!/usr/bin/env bash
# 集合化 V2 —— 真实 purchase 主过程 N 行三方基准（SP / Off / On）。
#
# 三方：openGauss 7.0.0-RC3 原生 PACKAGE PROCEDURE CALL（存过参照）/
#       SpinDuck Off（解释器逐行 RBAR）/ SpinDuck On（batch-atomic 集合化）。
# 同一份 N 行源数据（DuckDB generate_series → CSV → 参照 \copy 同源），
# 目标表 dat_trd_repurchase 全列排序后三方逐行一致性断言。
#
# 用法：
#   bash golden/v2_set_oriented_verify/run_bench.sh                  # SpinDuck Off/On（SP 需容器）
#   SPINDUCK_BIN=/path/to/spinduck bash ...                           # 复用已构建 CLI
#   N=1000 bash ...                                                   # 快速冒烟（默认 100000）
#   OG_CONTAINER=sd-og70 OG_LIBS=/opt/oglibs2 bash ...                # 启用 SP 参照方
#   ROUNDS_ON=3 bash ...                                              # On 轮数（默认 3）
#   KEEP_WORK=1 bash ...                                              # 保留临时目录
#
# 环境：Rust stable、cargo、git、python3；SP 参照需 docker +
#   opengauss/opengauss:7.0.0-RC3.B025（5.0.0 amd64 在 CALL 上 postmaster 自杀，V21）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REPORT_DIR="$SCRIPT_DIR/reports"
CORPUS="$REPO_ROOT/golden/case_corpus_purchase"
BIN="${SPINDUCK_BIN:-$REPO_ROOT/spinduck/target/debug/spinduck}"
N="${N:-100000}"
ROUNDS_ON="${ROUNDS_ON:-3}"
STEP_BUDGET="${STEP_BUDGET:-100000000}"
OG_CONTAINER="${OG_CONTAINER:-}"
OG_LIBS="${OG_LIBS:-/opt/oglibs2}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/v2_bench.XXXXXX")"
ROUTINE="pkg_split_trade_step3_sh.prc_trd_deal_purchase"

cleanup() {
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "[v2bench] 保留临时目录: $WORK";
  else rm -rf "$WORK"; fi
}
trap cleanup EXIT

sd_call() {  # $1=set-oriented 模式；stdout=业务输出，stderr=ELAPSED_MS=
  python3 - "$BIN" "$P" "$STEP_BUDGET" "$1" <<'PY'
import subprocess,sys,time
bin,p,budget,mode=sys.argv[1:5]
files=["sql/routines/0_SIMPLIFIED_fnc_com_getday.sql","sql/routines/0_SIMPLIFIED_fnc_com_if_deal_date.sql",
       "sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql","sql/routines/PKG_SPLIT_TRADE_STEP3_SH.sql",
       "sql/stubs/STUB_pkg_split_trade_log.sql"]
args=[bin,"call","--db",p+"/local.duckdb"]
for f in files: args+=["--file",f]
args+=["--step-budget",budget]
if mode!="none": args+=["--set-oriented="+mode]
args+=["pkg_split_trade_step3_sh.prc_trd_deal_purchase","'20250715'","null"]
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

echo "[v2bench] CLI: $BIN ($("$BIN" version 2>/dev/null || echo '?'))"
[ -x "$BIN" ] || { echo "[v2bench] 找不到 CLI：$BIN" >&2; exit 1; }

# ---- 1. SpinDuck 工程：逐字语料 + codeweb 标记 ----
P="$WORK/project"; mkdir -p "$P"
cp -R "$CORPUS/sql" "$P/sql"; cp -R "$CORPUS/rules" "$P/rules"
cat > "$P/spinduck.toml" <<'EOF'
[project]
name = "v2_bench"
kernel = "1.5.5"
[engine]
set_oriented = "batch-atomic"
[paths]
routines = "sql/routines"
ddl = "sql/ddl"
stubs = ["sql/stubs"]
rules = ["rules/spinduck-rules.toml"]
[[entry]]
routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase"
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

echo "[v2bench] compile --analyze"
"$BIN" compile --dir "$P" --analyze > "$WORK/compile.log" 2>&1 || true
GEN="$P/.spinduck/plan/$ROUTINE.toml"
[ -f "$GEN" ] || { echo "[v2bench] compile 未产出计划" >&2; cat "$WORK/compile.log" >&2; exit 1; }
python3 - "$GEN" "$P/.spinduck/plan/call_plan.toml" <<'PY'
import re,sys
t=open(sys.argv[1]).read()
t=re.sub(r'\[\[missing\]\][^\[]*','',t)
# codeweb 计划不含视图文件；运行期集合化证明需要 v_par_fund_check_para 实体，补入
t=t.replace('ddl_tables = ["sql/ddl/tables.sql"]',
            'ddl_tables = ["sql/ddl/tables.sql", "sql/ddl/views/view_check_para.sql"]')
open(sys.argv[2],'w').write(t)
PY
echo "[v2bench] deploy"
"$BIN" deploy "$P/.spinduck/plan/call_plan.toml" --db "$P/local.duckdb" > "$WORK/deploy.log" 2>&1 \
  || { echo "[v2bench] deploy 失败" >&2; cat "$WORK/deploy.log" >&2; exit 1; }
DB="$P/local.duckdb"

# ---- 2. 参数表（seed.sql 中除 mid_yjqs_detail 外的 INSERT，逐字）----
python3 - "$P" "$BIN" "$CORPUS" <<'PY'
import sys,subprocess,tempfile,os
p,bin,corpus=sys.argv[1:4]
sql=open(corpus+'/cases/datasets/seed.sql').read()
n=0
for s in sql.split(';'):
    u=s.strip().upper()
    if u.startswith('INSERT') and 'MID_YJQS_DETAIL' not in u:
        f=tempfile.NamedTemporaryFile('w',suffix='.sql',delete=False); f.write(s.strip()+';\n'); f.close()
        r=subprocess.run([bin,'query','--db',p+'/local.duckdb','--sql-file',f.name,'--out',f.name+'.json'],capture_output=True,text=True)
        os.unlink(f.name)
        if r.returncode!=0: sys.exit("参数载入失败: "+r.stderr+r.stdout)
        n+=1
print(f"[v2bench] 参数表载入 {n} 条 INSERT")
PY

# ---- 3. N 行同源种子（命中游标过滤 + F001 路径，Part B 同口径）----
cat > "$WORK/gen.sql" <<EOF
INSERT INTO mid_yjqs_detail (gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,yj01,fxj,accrual,pay_type,ghcjje,cjbh,yj02,yj03,yj04,stock_kind,yj06,yj05,sub_gddm,yj07,yj08,yj09,is_yysg,ywlb,etf_flag,check_type,date_source,ghcjje2)
SELECT 'A0001','X001','001','600000','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,100000+g,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000
FROM generate_series(1,$N) AS t(g);
EOF
"$BIN" query --db "$DB" --sql-file "$WORK/gen.sql" --out "$WORK/gen.json" >/dev/null
printf 'SELECT count(*) FROM mid_yjqs_detail;\n' > "$WORK/cnt.sql"
"$BIN" query --db "$DB" --sql-file "$WORK/cnt.sql" --out "$WORK/cnt.json" >/dev/null
SRC_N="$(python3 -c "import json;print(json.load(open('$WORK/cnt.json'))['rows'][0][0])")"
echo "[v2bench] 源种子行数: $SRC_N（目标 $N）"
[ "$SRC_N" = "$N" ] || { echo "[v2bench] 源行数不符" >&2; exit 1; }

printf "COPY (SELECT gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,yj01,fxj,accrual,pay_type,ghcjje,cjbh,yj02,yj03,yj04,stock_kind,yj06,yj05,sub_gddm,yj07,yj08,yj09,is_yysg,ywlb,etf_flag,check_type,date_source,ghcjje2 FROM mid_yjqs_detail ORDER BY cjbh) TO '%s/src.csv' (DELIMITER ',', HEADER);\n" "$WORK" > "$WORK/export.sql"
"$BIN" query --db "$DB" --sql-file "$WORK/export.sql" --out "$WORK/e.json" >/dev/null

# ---- 4. SpinDuck Off / On / Strict 测时 ----
printf 'DELETE FROM dat_trd_repurchase;\n' > "$WORK/trunc.sql"
printf 'SELECT * FROM dat_trd_repurchase ORDER BY security_id, partner_code, purchase_no;\n' > "$WORK/dump.sql"

echo "[v2bench] SpinDuck Off pass"
sd_call off > "$WORK/off_call.txt" 2> "$WORK/off_ms.txt"
OFF_MS="$(sed -n 's/ELAPSED_MS=//p' "$WORK/off_ms.txt")"
"$BIN" query --db "$DB" --sql-file "$WORK/dump.sql" --out "$WORK/off_target.json" >/dev/null

echo "[v2bench] SpinDuck On (batch-atomic) × $ROUNDS_ON"
declare -a ON_MS=()
for r in $(seq 1 "$ROUNDS_ON"); do
  "$BIN" query --db "$DB" --sql-file "$WORK/trunc.sql" --out "$WORK/t.json" >/dev/null
  sd_call batch-atomic > "$WORK/on_call_r${r}.txt" 2> "$WORK/on_ms_r${r}.txt"
  ON_MS+=("$(sed -n 's/ELAPSED_MS=//p' "$WORK/on_ms_r${r}.txt")")
  echo "[v2bench]   on round $r: ${ON_MS[$((r-1))]}ms"
done
ON_MED="$(median "${ON_MS[@]}")"
"$BIN" query --db "$DB" --sql-file "$WORK/trunc.sql" --out "$WORK/t.json" >/dev/null
sd_call batch-atomic > "$WORK/on_call.txt" 2>/dev/null
"$BIN" query --db "$DB" --sql-file "$WORK/dump.sql" --out "$WORK/on_target.json" >/dev/null

echo "[v2bench] SpinDuck Strict pass（预期 Skipped→回退）"
"$BIN" query --db "$DB" --sql-file "$WORK/trunc.sql" --out "$WORK/t.json" >/dev/null
sd_call strict > "$WORK/strict_call.txt" 2> "$WORK/strict_ms.txt"
STRICT_MS="$(sed -n 's/ELAPSED_MS=//p' "$WORK/strict_ms.txt")"
"$BIN" query --db "$DB" --sql-file "$WORK/dump.sql" --out "$WORK/strict_target.json" >/dev/null

# ---- 5. SP 参照方（可选，需容器）----
SP_MS=""; REF_N=""
if [ -n "$OG_CONTAINER" ]; then
  echo "[v2bench] SP 参照：$OG_CONTAINER 部署 + 载入 + CALL"
  docker exec "$OG_CONTAINER" mkdir -p /tmp/ref
  for f in tables.sql view_check_para.sql 0_SIMPLIFIED_fnc_com_getday.sql \
           0_SIMPLIFIED_fnc_com_if_deal_date.sql 2_FNC_GET_PURCHASE_JS_DAYS.sql \
           STUB_pkg_split_trade_log.sql PKG_SPLIT_TRADE_STEP3_SH.sql; do
    src=""
    for d in "$CORPUS/sql/ddl" "$CORPUS/sql/ddl/views" "$CORPUS/sql/routines" "$CORPUS/sql/stubs"; do
      [ -f "$d/$f" ] && src="$d/$f" && break
    done
    [ -n "$src" ] || { echo "[v2bench] 参照源缺失: $f" >&2; exit 1; }
    docker cp "$src" "$OG_CONTAINER:/tmp/ref/$f"
  done
  docker cp "$WORK/src.csv" "$OG_CONTAINER:/tmp/ref/src.csv"
  cat > "$WORK/gsql.sh" <<EOF
#!/bin/bash
export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$OG_LIBS
exec /usr/local/opengauss/bin/gsql -U gaussdb -W "\$GS_PASSWORD" -d postgres -p 5432 -v ON_ERROR_STOP=1 -tA "\$@"
EOF
  docker cp "$WORK/gsql.sh" "$OG_CONTAINER:/tmp/ref/gsql.sh"
  docker exec "$OG_CONTAINER" bash -lc 'chmod +x /tmp/ref/gsql.sh; for f in tables.sql view_check_para.sql 0_SIMPLIFIED_fnc_com_getday.sql 0_SIMPLIFIED_fnc_com_if_deal_date.sql 2_FNC_GET_PURCHASE_JS_DAYS.sql STUB_pkg_split_trade_log.sql PKG_SPLIT_TRADE_STEP3_SH.sql; do /tmp/ref/gsql.sh -f /tmp/ref/$f >/dev/null || exit 1; done' \
    || { echo "[v2bench] 参照侧例程部署失败" >&2; exit 1; }
  python3 - "$CORPUS" "$WORK" <<'PY'
import sys
sql=open(sys.argv[1]+'/cases/datasets/seed.sql').read()
out=[]
for s in sql.split(';'):
    u=s.strip().upper()
    if u.startswith('INSERT') and 'MID_YJQS_DETAIL' not in u:
        out.append(s.strip()+';')
open(sys.argv[2]+'/seeds.sql','w').write('\n'.join(out)+'\n')
PY
  docker cp "$WORK/seeds.sql" "$OG_CONTAINER:/tmp/ref/seeds.sql"
  docker exec "$OG_CONTAINER" bash -lc '/tmp/ref/gsql.sh -f /tmp/ref/seeds.sql >/dev/null'
  COLS="gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,yj01,fxj,accrual,pay_type,ghcjje,cjbh,yj02,yj03,yj04,stock_kind,yj06,yj05,sub_gddm,yj07,yj08,yj09,is_yysg,ywlb,etf_flag,check_type,date_source,ghcjje2"
  printf "\\\\copy mid_yjqs_detail(%s) FROM '/tmp/ref/src.csv' WITH (FORMAT csv, HEADER true, DELIMITER ',');\nSELECT count(*) FROM mid_yjqs_detail;\n" "$COLS" > "$WORK/loadcsv.sql"
  docker cp "$WORK/loadcsv.sql" "$OG_CONTAINER:/tmp/ref/loadcsv.sql"
  REF_N="$(docker exec "$OG_CONTAINER" bash -lc '/tmp/ref/gsql.sh -f /tmp/ref/loadcsv.sql' | tail -1 | tr -d '[:space:]')"
  echo "[v2bench] 参照源行数: $REF_N"
  [ "$REF_N" = "$N" ] || { echo "[v2bench] 参照源行数不符" >&2; exit 1; }
  printf "CALL %s('20250715', NULL);\n" "$ROUTINE" > "$WORK/call_ref.sql"
  docker cp "$WORK/call_ref.sql" "$OG_CONTAINER:/tmp/ref/call_ref.sql"
  SP_MS="$(python3 - "$OG_CONTAINER" <<'PY'
import subprocess,sys,time
c=sys.argv[1]
t=time.perf_counter()
r=subprocess.run(["docker","exec",c,"bash","-lc","/tmp/ref/gsql.sh -f /tmp/ref/call_ref.sql"],capture_output=True,text=True)
ms=(time.perf_counter()-t)*1000
sys.stderr.write(r.stderr[-300:])
if r.returncode!=0:
    sys.stderr.write(r.stdout[-800:]); sys.exit(r.returncode)
print(f"{ms:.1f}")
PY
)"
  echo "[v2bench] SP CALL: ${SP_MS}ms"
  printf "\\\\copy (SELECT * FROM dat_trd_repurchase ORDER BY security_id, partner_code, purchase_no) TO '/tmp/ref/tgt_ref.csv' WITH (FORMAT csv, DELIMITER ',');\n" > "$WORK/export_ref.sql"
  docker cp "$WORK/export_ref.sql" "$OG_CONTAINER:/tmp/ref/export_ref.sql"
  docker exec "$OG_CONTAINER" bash -lc '/tmp/ref/gsql.sh -f /tmp/ref/export_ref.sql >/dev/null'
  docker cp "$OG_CONTAINER:/tmp/ref/tgt_ref.csv" "$WORK/tgt_ref.csv"
fi

# ---- 6. noop 固定开销 ----
cat > "$P/noop.sql" <<'EOF'
CREATE OR REPLACE PROCEDURE prc_noop IS BEGIN NULL; END;
/
EOF
declare -a NOOP_MS=()
for r in 1 2 3; do
  NOOP_MS+=("$(python3 - "$BIN" "$P" <<'PY'
import subprocess,sys,time
bin,p=sys.argv[1:3]
t=time.perf_counter()
subprocess.run([bin,"call","--db",p+"/local.duckdb","--file",p+"/noop.sql","prc_noop"],cwd=p,capture_output=True)
print(f"{(time.perf_counter()-t)*1000:.1f}")
PY
)")
done
NOOP_MED="$(median "${NOOP_MS[@]}")"

# ---- 7. 一致性断言 ----
mkdir -p "$REPORT_DIR"
python3 - "$WORK/off_target.json" "$WORK/on_target.json" "$WORK/strict_target.json" \
           "$WORK/tgt_ref.csv" "$N" > "$WORK/consistency.txt" 2>&1 <<'PY' || true
import json,csv,sys,os
off=json.load(open(sys.argv[1])); on=json.load(open(sys.argv[2])); strict=json.load(open(sys.argv[3]))
n=int(sys.argv[4]); refcsv=sys.argv[5]
def norm(rows): return sorted(tuple('' if v is None else str(v) for v in r) for r in rows)
sd_off,sd_on,sd_strict=norm(off['rows']),norm(on['rows']),norm(strict['rows'])
print(f"rows: off={len(sd_off)} on={len(sd_on)} strict={len(sd_strict)} expect={n}")
print(f"off==on: {sd_off==sd_on}")
print(f"on==strict: {sd_on==sd_strict}")
ok = sd_off==sd_on==sd_strict and len(sd_off)==n
if os.path.exists(refcsv):
    rf=norm([r for r in csv.reader(open(refcsv))])
    print(f"ref rows={len(rf)}")
    print(f"off==ref: {sd_off==rf}")
    print(f"on==ref : {sd_on==rf}")
    ok = ok and sd_off==rf
else:
    print("ref: 未提供（OG_CONTAINER 未设），仅 SpinDuck 内部一致")
print("CONSISTENT=" + ("OK" if ok else "FAIL"))
PY
cat "$WORK/consistency.txt"
grep -q "CONSISTENT=OK" "$WORK/consistency.txt" || { echo "[v2bench] 一致性断言失败" >&2; exit 1; }

# ---- 8. 汇总 ----
SPEEDUP_OFF_ON=$(python3 -c "print(f'{$OFF_MS/$ON_MED:.1f}')")
if [ -n "$SP_MS" ]; then SPEEDUP_SP_ON=$(python3 -c "print(f'{$SP_MS/$ON_MED:.1f}')"); else SPEEDUP_SP_ON="n/a"; fi
{
  echo "=== 集合化 V2 真实 purchase 主过程 ${N} 三方基准 ==="
  echo "CLI: $BIN  构建: debug  step-budget: $STEP_BUDGET"
  echo "过程: $ROUTINE（真实语料逐字）；参照: openGauss 7.0.0-RC3 ${OG_CONTAINER:-<未启用>}"
  echo "数据: $N 行同源（SpinDuck generate_series → CSV → \\copy），源行数 SpinDuck=$SRC_N 参照=${REF_N:-n/a}"
  echo ""
  echo "方 | 中位墙钟(ms) | 各轮(ms) | 口径"
  echo "SP 存过（openGauss 原生 CALL） | ${SP_MS:-n/a} | - | gsql 客户端墙钟"
  echo "SpinDuck Off（解释器游标 RBAR） | $OFF_MS | $OFF_MS | CLI call 端到端"
  echo "SpinDuck On（batch-atomic 集合化） | $ON_MED | ${ON_MS[*]} | CLI call 端到端"
  echo "SpinDuck Strict | $STRICT_MS | - | CLI call 端到端（Skipped→回退）"
  echo ""
  echo "SpinDuck 固定开销（noop call 中位）: $NOOP_MED ms（${NOOP_MS[*]}）"
  echo "Off→On 加速比: ${SPEEDUP_OFF_ON}×；SP→On: ${SPEEDUP_SP_ON}×"
  echo ""
  echo "集合化判定:"
  grep '^SET_ORIENTED' "$WORK/on_call.txt" 2>/dev/null | sed 's/^/  On: /' || echo "  On: <无报告行>"
  grep '^SET_ORIENTED' "$WORK/off_call.txt" 2>/dev/null | sed 's/^/  Off: /' || echo "  Off: <无报告行（feature 关）>"
  grep '^SET_ORIENTED' "$WORK/strict_call.txt" 2>/dev/null | sed 's/^/  Strict: /' || echo "  Strict: <无报告行>"
  echo ""
  echo "数据一致性（dat_trd_repurchase 全列排序逐行）:"
  sed 's/^/  /' "$WORK/consistency.txt"
  echo ""
  echo "口径: SpinDuck 为 CLI call 端到端墙钟（含固定开销 ~$NOOP_MED ms）；"
  echo "参照为 gsql 客户端墙钟（含连接开销）。debug 构建绝对值仅证明方向与量级。"
  echo "Off/Strict 单轮（百秒级成本），On $ROUNDS_ON 轮取中位。"
} > "$REPORT_DIR/bench_summary.log"
cp "$WORK/on_call.txt" "$REPORT_DIR/v2_on_call.txt"
cp "$WORK/off_call.txt" "$REPORT_DIR/v2_off_call.txt"
cp "$WORK/strict_call.txt" "$REPORT_DIR/v2_strict_call.txt" 2>/dev/null || true
[ -f "$WORK/tgt_ref.csv" ] && cp "$WORK/tgt_ref.csv" "$REPORT_DIR/tgt_ref.csv"
cat "$REPORT_DIR/bench_summary.log"
echo "[v2bench] 完成。报告: $REPORT_DIR/bench_summary.log"
