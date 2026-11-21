#!/usr/bin/env bash
# 集合化 V3 M2 —— prc_trd_hz_byfund 100k 三方黄金对账（docs/goals/set-oriented-v3.md 第 5 行）。
#
# 三方：openGauss 7.0.0-RC3 原生 PACKAGE PROCEDURE CALL（参照）/
#       SpinDuck Off（解释器逐行）/ SpinDuck BatchAtomic（区域0集合化）。
# 同一份 N 行源数据（generate_series，DuckDB 与 gsql 两侧同文件执行），
# 四张目标表全列排序逐行比对：dat_fund_cjqs / dat_fund_xw_venture /
# mid_yjqs_detail(cjbh,fund_code,main_xwdm) / qfqscjhbrbb_temp。
# 通过后写 docs/goals/artifacts/set-oriented-v3-golden.json。
#
# 用法：
#   bash golden/set_oriented_v3_golden/run_golden.sh
#   SPINDUCK_BIN=/path/to/spinduck N=1000 OG_CONTAINER=sd-og70 bash ...   # 冒烟
#   N=100000 bash ...                                                    # 完整（默认）
#   SKIP_GAUSS=1 bash ...                                                # 仅 SpinDuck 双方（artifact 不写）
#
# 环境：Rust stable、cargo、python3；参照需 docker +
#   opengauss/opengauss:7.0.0-RC3.B025 容器（5.0.0 amd64 在 CALL 上 postmaster 自杀）。
#   容器沿用 sd-og70 形态：GS_PASSWORD=Change@me1, -p 127.0.0.1:5435:5432,
#   /opt/oglibs2 只读挂载（LAPACK/OpenBLAS 依赖）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CORPUS="$REPO_ROOT/golden/case_corpus_purchase"
PKG_SQL="$SCRIPT_DIR/sql"
ARTIFACT="$REPO_ROOT/docs/goals/artifacts/set-oriented-v3-golden.json"
BIN="${SPINDUCK_BIN:-$REPO_ROOT/spinduck/target/debug/spinduck}"
# 未显式指定二进制时自行构建：artifact 的 git_head 记录 HEAD，
# 被测二进制必须与该版本源码一致（评审口径）。
if [ -z "${SPINDUCK_BIN:-}" ]; then
  (cd "$REPO_ROOT/spinduck" && cargo build --features cli --quiet) \
    || { echo "[v3golden] CLI 构建失败" >&2; exit 1; }
fi
N="${N:-100000}"
STEP_BUDGET="${STEP_BUDGET:-100000000}"
OG_CONTAINER="${OG_CONTAINER:-sd-og70}"
OG_LIBS="${OG_LIBS:-/opt/oglibs2}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/v3golden.XXXXXX")"
ROUTINE="pkg_split_trade_step3_sh.prc_trd_hz_byfund"
CALL_DATE="20250715"

cleanup() {
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "[v3golden] 保留临时目录: $WORK";
  else rm -rf "$WORK"; fi
}
trap cleanup EXIT

# 语料 routines 中被 9 个确定性桩替换的真实函数（PR#56 同口径：
# 桩两侧同源、消除函数移植差异对集合化对账的干扰）。
replaced() {
  case "$(basename "$1")" in
    1_FNC_COM_GET_FUND_CODE.sql|1_FNC_COM_GET_MAIN_XWDM.sql|\
    1_FNC_COM_GET_SEAT_FEE_TYPE.sql|1_FNC_COM_GET_SEAT_PARA.sql|\
    0_SIMPLIFIED_fnc_get_bond_yjlx.sql|1_FNC_GET_FUND_DATA_MODE.sql|\
    1_FNC_GET_RATE_INFO2.sql|1_FNC_GET_SPLIT_DATE.sql|1_FNC_GET_STOCK_KIND.sql) return 0;;
    *) return 1;;
  esac
}

# 例程文件清单（标量函数在前、包在后、日志桩最后——宏按注册顺序生成）
routine_files() {
  local R="$CORPUS/sql/routines" S="$PKG_SQL/routines"
  for f in "$R"/*.sql; do
    replaced "$f" && continue
    case "$(basename "$f")" in PKG_*) continue;; esac
    printf '%s\n' "$f"
  done
  printf '%s\n' "$PKG_SQL/routines/"0_STUB_*.sql
  printf '%s\n' "$R/PKG_SPLIT_TRADE_STEP3_SH.sql"
  printf '%s\n' "$CORPUS/sql/stubs/STUB_pkg_split_trade_log.sql"
}

sd_call() {  # $1=off|batch-atomic → stdout call 输出
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

echo "[v3golden] CLI: $BIN ($("$BIN" version 2>/dev/null || echo '?'))"
[ -x "$BIN" ] || { echo "[v3golden] 找不到 CLI：$BIN" >&2; exit 1; }

# ---- 1. SpinDuck 工程：逐字语料 + 桩替换 + 补丁 DDL ----
P="$WORK/project"; mkdir -p "$P"
cp -R "$CORPUS/sql" "$P/sql"; cp -R "$CORPUS/rules" "$P/rules" 2>/dev/null || true
# 移除被桩替换的 9 个真实函数文件
for f in "$P"/sql/routines/*.sql; do
  replaced "$f" && rm "$f"
done
cp "$PKG_SQL/routines/"0_STUB_*.sql "$P/sql/routines/"
cp "$PKG_SQL/patch_ddl.sql" "$P/sql/ddl/"
cat > "$P/spinduck.toml" <<'EOF'
[project]
name = "v3golden"
kernel = "1.5.5"
[engine]
threads = 4
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

echo "[v3golden] compile --analyze"
"$BIN" compile --dir "$P" --analyze > "$WORK/compile.log" 2>&1 || true
GEN="$P/.spinduck/plan/$ROUTINE.toml"
[ -f "$GEN" ] || { echo "[v3golden] compile 未产出计划" >&2; tail -20 "$WORK/compile.log" >&2; exit 1; }
python3 - "$GEN" "$P/.spinduck/plan/call_plan.toml" <<'PY'
import re,sys
t=open(sys.argv[1]).read()
t=re.sub(r'\[\[missing\]\][^\[]*','',t)
# ddl 顺序：tables → 视图 → patch（ALTER 依赖 par_fund_seat_para）
m=re.search(r'ddl_tables = \[(.*?)\]',t)
if m:
    files=re.findall(r'"([^"]+)"',m.group(1))
    pri={'tables.sql':0,'patch_ddl.sql':9}
    def key(f):
        b=f.rsplit('/',1)[-1]
        if b=='tables.sql': return 0
        if b=='patch_ddl.sql': return 9
        return 1 if '/views/' in f else 5
    files.sort(key=key)
    t=t[:m.start()]+'ddl_tables = ['+', '.join('"%s"'%f for f in files)+']'+t[m.end():]
open(sys.argv[2],'w').write(t)
PY
echo "[v3golden] deploy"
"$BIN" deploy "$P/.spinduck/plan/call_plan.toml" --db "$P/local.duckdb" > "$WORK/deploy.log" 2>&1 \
  || { echo "[v3golden] deploy 失败" >&2; cat "$WORK/deploy.log" >&2; exit 1; }
DB="$P/local.duckdb"

# ---- 2. 参数种子 + N 行明细（两侧同文件）----
python3 - "$PKG_SQL/seeds_params.sql" "$WORK/seeds_stmts" <<'PY'
import sys,os
sql=open(sys.argv[1]).read()
os.makedirs(sys.argv[2],exist_ok=True)
parts=[s.strip() for s in sql.split(';') if s.strip()]
for i,s in enumerate(parts):
    open(f"{sys.argv[2]}/s{i:02d}.sql",'w').write(s+';\n')
print(f"[v3golden] 参数种子语句数: {len(parts)}")
PY
for f in "$WORK"/seeds_stmts/*.sql; do
  "$BIN" query --db "$DB" --sql-file "$f" --out "$WORK/q.json" >/dev/null
done
sed "s/__N__/$N/" "$PKG_SQL/gen_detail.sql" > "$WORK/gen.sql"
"$BIN" query --db "$DB" --sql-file "$WORK/gen.sql" --out "$WORK/gen.json" >/dev/null
printf 'SELECT count(*) FROM mid_yjqs_detail;\n' > "$WORK/cnt.sql"
"$BIN" query --db "$DB" --sql-file "$WORK/cnt.sql" --out "$WORK/cnt.json" >/dev/null
SRC_N="$(python3 -c "import json;print(json.load(open('$WORK/cnt.json'))['rows'][0][0])")"
echo "[v3golden] 源行数: ${SRC_N}（目标 ${N}）"
[ "$SRC_N" = "$N" ] || { echo "[v3golden] 源行数不符" >&2; exit 1; }

# ---- 3. SpinDuck Off / BatchAtomic ----
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

echo "[v3golden] SpinDuck Off pass（100k 解释器，分钟级）"
sd_call off > "$WORK/off_call.txt" 2> "$WORK/off_ms.txt"
OFF_MS="$(sed -n 's/ELAPSED_MS=//p' "$WORK/off_ms.txt")"
dump_all off

echo "[v3golden] SpinDuck BatchAtomic pass"
"$BIN" query --db "$DB" --sql-file "$WORK/tr_cjqs.sql" --out "$WORK/t.json" >/dev/null
"$BIN" query --db "$DB" --sql-file "$WORK/tr_vent.sql" --out "$WORK/t.json" >/dev/null
sd_call batch-atomic > "$WORK/on_call.txt" 2> "$WORK/on_ms.txt"
ON_MS="$(sed -n 's/ELAPSED_MS=//p' "$WORK/on_ms.txt")"
dump_all on

# ---- 4. openGauss 参照方 ----
REF_N=""
if [ "${SKIP_GAUSS:-0}" != "1" ]; then
  echo "[v3golden] SP 参照：$OG_CONTAINER 部署 + 载入 + CALL"
  # 注意：参数必须经 bash -lc 的位置参数透传（`... "$@"' name "$@"`），
  # 否则内层 $@ 为空、gsql 裸跑读空 stdin 直接 0 退出——所有部署会"成功"但什么都没执行。
  gsql_exec() { docker exec "$OG_CONTAINER" bash -lc \
    'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:'"$OG_LIBS"'; /usr/local/opengauss/bin/gsql -U gaussdb -W "$GS_PASSWORD" -d hz -v ON_ERROR_STOP=1 "$@"' gsql "$@"; }
  # 干净库：重建 hz schema 对象（容器为专用参照实例，直接 DROP/CREATE DATABASE；
  # 先终止残留连接，否则 DROP 被 "database is being accessed" 静默跳过）
  docker exec "$OG_CONTAINER" bash -lc \
    "export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$OG_LIBS; /usr/local/opengauss/bin/gsql -U gaussdb -W \"\$GS_PASSWORD\" -d postgres -v ON_ERROR_STOP=1 \
     -c \"SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='hz' AND pid <> pg_backend_pid()\" \
     -c 'DROP DATABASE IF EXISTS hz'" || { echo "[v3golden] 参照库 DROP 失败" >&2; exit 1; }
  docker exec "$OG_CONTAINER" bash -lc \
    "export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$OG_LIBS; /usr/local/opengauss/bin/gsql -U gaussdb -W \"\$GS_PASSWORD\" -d postgres -v ON_ERROR_STOP=1 -c 'CREATE DATABASE hz'" || \
    { echo "[v3golden] 参照库 CREATE 失败" >&2; exit 1; }
  # DDL：tables → views → patch
  for f in "$CORPUS/sql/ddl/tables.sql" "$CORPUS"/sql/ddl/views/*.sql "$PKG_SQL/patch_ddl.sql"; do
    docker cp "$f" "$OG_CONTAINER:/tmp/hz_$(basename "$f")"
    gsql_exec -f "/tmp/hz_$(basename "$f")" >/dev/null \
      || { echo "[v3golden] 参照侧 DDL 失败 $f" >&2; exit 1; }
  done
  # 依赖包桩先行：openGauss 编译期即解析 pkg_split_trade_log.inst_log，
  # files.txt 中该桩排在主包之后（SpinDuck 注册序无此约束），参照侧需前置。
  f="$CORPUS/sql/stubs/STUB_pkg_split_trade_log.sql"
  docker cp "$f" "$OG_CONTAINER:/tmp/hz_$(basename "$f")"
  gsql_exec -f "/tmp/hz_$(basename "$f")" >/dev/null \
    || { echo "[v3golden] 参照侧依赖包桩部署失败" >&2; exit 1; }
  # 例程：与 SpinDuck files.txt 同序
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    docker cp "$f" "$OG_CONTAINER:/tmp/hz_$(basename "$f")"
    gsql_exec -f "/tmp/hz_$(basename "$f")" >/dev/null \
      || { echo "[v3golden] 参照侧例程部署失败 $f" >&2; exit 1; }
  done < "$WORK/files.txt"
  # 参数种子 + N 行明细（同文件）
  for f in "$WORK"/seeds_stmts/*.sql; do
    docker cp "$f" "$OG_CONTAINER:/tmp/hz_seed_one.sql"
    gsql_exec -f /tmp/hz_seed_one.sql >/dev/null || { echo "[v3golden] 参照种子失败 $f" >&2; exit 1; }
  done
  sed "s/__N__/$N/" "$PKG_SQL/gen_detail.sql" > "$WORK/hz_gen.sql"
  docker cp "$WORK/hz_gen.sql" "$OG_CONTAINER:/tmp/hz_gen.sql"
  gsql_exec -f /tmp/hz_gen.sql >/dev/null \
    || { echo "[v3golden] 参照侧明细生成失败" >&2; exit 1; }
  REF_N="$(gsql_exec -tA -c 'SELECT count(*) FROM mid_yjqs_detail' | tr -d '[:space:]')"
  echo "[v3golden] 参照源行数: $REF_N"
  [ "$REF_N" = "$N" ] || { echo "[v3golden] 参照源行数不符" >&2; exit 1; }
  printf "CALL %s('%s', NULL);\n" "$ROUTINE" "$CALL_DATE" > "$WORK/call_ref.sql"
  docker cp "$WORK/call_ref.sql" "$OG_CONTAINER:/tmp/hz_call_ref.sql"
  SP_T0=$(python3 -c 'import time;print(time.time())')
  gsql_exec -f /tmp/hz_call_ref.sql > "$WORK/ref_call.txt" 2>&1 \
    || { echo "[v3golden] 参照 CALL 失败" >&2; tail -20 "$WORK/ref_call.txt" >&2; exit 1; }
  SP_MS="$(python3 -c "import time;print(f'{(time.time()-$SP_T0)*1000:.1f}')")"
  echo "[v3golden] SP CALL: ${SP_MS}ms"
  # 参照导出（CSV → 归一化比对）
  cat > "$WORK/export_ref.sql" <<'EOF'
\copy (SELECT * FROM dat_fund_cjqs ORDER BY fund_code,xwdm,security_id,bs,trade_type,bcrq,scdm,gddm,gddmzm,check_type,cjbz,stock_kind,pay_type) TO '/tmp/hz_ref_cjqs.csv' WITH (FORMAT csv, DELIMITER ',');
\copy (SELECT * FROM dat_fund_xw_venture ORDER BY fund_code,xwdm,trade_type,venture_type,scdm,operation_kind,bcrq) TO '/tmp/hz_ref_vent.csv' WITH (FORMAT csv, DELIMITER ',');
\copy (SELECT cjbh,fund_code,main_xwdm FROM mid_yjqs_detail ORDER BY cjbh) TO '/tmp/hz_ref_mid.csv' WITH (FORMAT csv, DELIMITER ',');
\copy (SELECT * FROM qfqscjhbrbb_temp ORDER BY gddm,gddmzm,gsdm,bcrq,bs,zqdm,seq_no) TO '/tmp/hz_ref_qf.csv' WITH (FORMAT csv, DELIMITER ',');
EOF
  docker cp "$WORK/export_ref.sql" "$OG_CONTAINER:/tmp/hz_export.sql"
  gsql_exec -f /tmp/hz_export.sql >/dev/null
  for t in cjqs vent mid qf; do
    docker cp "$OG_CONTAINER:/tmp/hz_ref_$t.csv" "$WORK/ref_$t.csv" \
      || { echo "[v3golden] 参照导出缺失 $t" >&2; exit 1; }
  done
fi

# ---- 5. 一致性断言 + artifact ----
python3 - "$WORK" "$N" "$BIN" "$OFF_MS" "${ON_MS:-0}" "${SP_MS:-0}" "$OG_CONTAINER" \
  "${SKIP_GAUSS:-0}" \
  > "$WORK/consistency.txt" 2>&1 <<'PY' || true
import json,csv,sys,os,re
from decimal import Decimal
w=sys.argv[1]; n=int(sys.argv[2])
# 参照模式（SKIP_GAUSS!=1）下四份参照 CSV 必须全部存在且参与比对，
# 缺失即 FAIL——不得以 Off==On 冒充三方一致。
ref_mode = len(sys.argv) > 8 and sys.argv[8] != '1'
# 数值单元格归一：openGauss 数字文本输出省略前导零（.50），SpinDuck 输出 0.50；
# 值等价故按 Decimal 规范化。前导零整数（如 '0501' 编码列）保持原文不动。
NUM=re.compile(r'^[+-]?(\d+\.?\d*|\.\d+)$')
LEAD0=re.compile(r'^[+-]?0\d')
def canon(v):
    if v is None: return ''
    s=str(v)
    if NUM.match(s) and not LEAD0.match(s):
        try: return format(Decimal(s).normalize(),'f')
        except Exception: return s
    return s
def norm_json(p):
    d=json.load(open(p)); return sorted(tuple(canon(v) for v in r) for r in d['rows'])
def norm_csv(p):
    if not os.path.exists(p): return None
    return sorted(tuple(canon(v) for v in r) for r in csv.reader(open(p)))
ok=True; detail=[]
for t in ['cjqs','vent','mid','qf']:
    off=norm_json(f'{w}/off_{t}.json'); on=norm_json(f'{w}/on_{t}.json')
    ref=norm_csv(f'{w}/ref_{t}.csv')
    line=f"{t}: rows off={len(off)} on={len(on)}" + (f" ref={len(ref)}" if ref is not None else " ref=<无>")
    eq = off==on
    line+=f"  off==on:{eq}"
    if ref is not None:
        line+=f"  off==ref:{off==ref} on==ref:{on==ref}"
        eq = eq and off==ref
    elif ref_mode:
        line+="  ref=<缺失>"
        eq = False
    print(line); ok = ok and eq
print("CONSISTENT=" + ("OK" if ok else "FAIL"))
PY
cat "$WORK/consistency.txt"
grep -q "CONSISTENT=OK" "$WORK/consistency.txt" || { echo "[v3golden] 一致性断言失败" >&2; exit 1; }

# p_o_succeed：两侧 call 输出均须为 '0'（成功路径）
grep -q "OUT p_o_succeed = 0" "$WORK/off_call.txt" || { echo "[v3golden] Off p_o_succeed 非 0" >&2; cat "$WORK/off_call.txt"; exit 1; }
grep -q "OUT p_o_succeed = 0" "$WORK/on_call.txt" || { echo "[v3golden] BatchAtomic p_o_succeed 非 0" >&2; cat "$WORK/on_call.txt"; exit 1; }
grep -q "SET_ORIENTED.*converted" "$WORK/on_call.txt" || { echo "[v3golden] BatchAtomic 未 Converted" >&2; grep SET_ORIENTED "$WORK/on_call.txt" || true; exit 1; }

if [ "${SKIP_GAUSS:-0}" != "1" ]; then
  HEAD="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  python3 - "$ARTIFACT" "$HEAD" "$N" <<'PY'
import json,sys
path,head,n=sys.argv[1],sys.argv[2],int(sys.argv[3])
doc={
 "git_head":head,
 "input_rows":n,
 "p_o_succeed":0,
 "off_matches_gauss":True,
 "batch_atomic_matches_gauss":True,
 "method":"prc_trd_hz_byfund 100k 三方对账：openGauss 7.0.0-RC3 CALL vs "
         "SpinDuck Off vs BatchAtomic；四表全列排序逐行全等；"
         "9 个依赖函数以确定性桩替换（桩两侧同源，非真实依赖链全量验收）；"
         "run_golden.sh 复现",
}
import os; os.makedirs(os.path.dirname(path),exist_ok=True)
open(path,'w').write(json.dumps(doc,ensure_ascii=False,indent=2)+"\n")
print(path)
PY
fi

SPEEDUP=$(python3 -c "print(f'{$OFF_MS/$ON_MS:.1f}')" 2>/dev/null || echo n/a)
{
  echo "=== 集合化 V3 byfund ${N} 三方黄金对账 ==="
  echo "CLI: $BIN  step-budget: $STEP_BUDGET"
  echo "过程: ${ROUTINE}（逐字语料 + 9 确定性桩 + patch_ddl，见 sql/）"
  echo "参照: openGauss 7.0.0-RC3 ${OG_CONTAINER:-<未启用>}"
  echo "数据: mid_yjqs_detail $N 行同源（generate_series 互质乘子），SpinDuck=$SRC_N 参照=${REF_N:-n/a}"
  echo ""
  echo "SpinDuck Off:        ${OFF_MS}ms"
  echo "SpinDuck BatchAtomic:${ON_MS}ms   (Off→On ${SPEEDUP}×)"
  echo "SP CALL:             ${SP_MS:-n/a}ms"
  echo ""
  echo "集合化判定:"
  grep '^SET_ORIENTED' "$WORK/on_call.txt" 2>/dev/null | sed 's/^/  On: /' || echo "  On: <无报告行>"
  echo ""
  echo "一致性（四表全列排序逐行）:"
  sed 's/^/  /' "$WORK/consistency.txt"
} | tee "$WORK/summary.log"
echo "[v3golden] 完成。"
