#!/usr/bin/env bash
# M0 剖面：prc_trd_hz_byfund 四段实测（prefix / c_get_data / c_fxj / qfii），
# debug 与 release 各一组，产物 docs/goals/artifacts/set-oriented-v3-m0-profile.json。
#
# 口径（docs/goals/set-oriented-v3.md 第 2 行 + 设计 §5.2/§6 M0）：
#   - 分段靠"截断变体例程"：gen_variants.py 从退役语料的逐字副本里截出
#     prc_m0_noop / prc_m0_prefix / prc_m0_cgetdata / prc_m0_cfxj，
#     段耗时 = 相邻变体端到端墙钟之差（每段都是解释器真实执行的代码路径，
#     不是把总墙钟拆开）；qfii = 完整例程 − cfxj 变体。
#   - 数据：case_corpus_hz_byfund 退役语料（git 4327506^）+ 附录 A 三个外部桩
#     + QFII 开启种子（@DETAIL_N@ / @QFII_N@ 行）。
#   - 每段 ROUNDS 轮取中位数；每轮从种子模板库 cp 一份，保证段间状态一致。
#
# 用法：
#   bash golden/set_oriented_v3_m0/run_m0_profile.sh
#   DETAIL_N=5000 QFII_N=5000 ROUNDS=3 STEP_BUDGET=100000000 \
#     DEBUG_BIN=... RELEASE_BIN=... bash golden/set_oriented_v3_m0/run_m0_profile.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/golden/set_oriented_v3_m0"
CORPUS_REV="4327506^"   # case_corpus_hz_byfund 退役前最后状态
WORK="${M0_WORK:-/tmp/spinduck_m0_profile}"
P="$WORK/project"
DB="$P/local.duckdb"
TEMPLATE_DB="$WORK/seeded.duckdb"
DETAIL_N="${DETAIL_N:-5000}"
QFII_N="${QFII_N:-5000}"
ROUNDS="${ROUNDS:-3}"
STEP_BUDGET="${STEP_BUDGET:-100000000}"
CALL_DATE="'20250715'"
export RUST_MIN_STACK=268435456   # debug codeweb --analyze 递归需要

log() { echo "[m0] $*" >&2; }

# ---- 1. 物化工程 ----
if [ ! -d "$P/sql" ]; then
  log "materialize $CORPUS_REV:golden/case_corpus_hz_byfund → $P"
  rm -rf "$WORK"; mkdir -p "$WORK"
  git -C "$ROOT" archive "$CORPUS_REV" golden/case_corpus_hz_byfund \
    | tar -x -C "$WORK"
  mv "$WORK/golden/case_corpus_hz_byfund" "$P"
  rm -rf "$WORK/golden"

  # 三个外部函数：引擎内无实现 → 用设计附录 A 桩替换（桩正文逐字取自设计文档）
  rm -f "$P"/sql/routines/1_FNC_COM_GET_SEAT_PARA.sql \
        "$P"/sql/routines/1_FNC_GET_FUND_DATA_MODE.sql \
        "$P"/sql/routines/1_FNC_GET_RATE_INFO2.sql
  cp "$HERE"/sql/stubs_a/*.sql "$P/sql/routines/"

  # 分段变体（截断副本，登记为独立例程）
  python3 "$HERE/gen_variants.py" "$P/sql/routines/PKG_SPLIT_TRADE_STEP3_SH.sql" \
    > "$P/sql/routines/0_M0_VARIANTS.sql"

  # 兼容性补丁（只影响测量工程，不改引擎/语料语义）：
  #  a) par_fund_bond_accrual_type.tm 的 DEFAULT 用 GaussDB 方言 "sysdate"()，
  #     DuckDB 侧改写器不认引号内函数名 → 换成等价 current_timestamp。
  #  b) fnc_com_get_main_xwdm 依赖的 par_fund_account_member /
  #     par_oper_account_member 在任何语料里都没有 DDL（原语料靠异常分支兜底）；
  #     补最小空表让 codeweb 闭包完整、缺件门放行，行为与异常分支一致（v_count=0）。
  python3 - "$P" <<'PY'
import sys,os
p=sys.argv[1]
f=os.path.join(p,'sql/ddl/tables/par_fund_bond_accrual_type.sql')
s=open(f).read()
s=s.replace("""DEFAULT to_char("sysdate"(), 'yyyymmdd hh24:mi:ss'::text)""",
            "DEFAULT cast(current_timestamp as varchar)")
open(f,'w').write(s)
open(os.path.join(p,'sql/ddl/tables/par_fund_account_member.sql'),'w').write(
"""CREATE TABLE par_fund_account_member (
    fund_code character varying(10),
    member_code character varying(20),
    zjzh character varying(50),
    inure_begin_date character varying(8),
    inure_end_date character varying(8)
);
""")
open(os.path.join(p,'sql/ddl/tables/par_oper_account_member.sql'),'w').write(
"""CREATE TABLE par_oper_account_member (
    member_code character varying(20),
    member_type character varying(2),
    inure_begin_date character varying(8),
    inure_end_date character varying(8)
);
""")
PY

  cat > "$P/spinduck.toml" <<'EOF'
[project]
name = "m0_hz_byfund"
kernel = "1.5.5"
[engine]
threads = 4
[paths]
routines = "sql/routines"
ddl = "sql/ddl"
rules = "rules/spinduck-rules.toml"
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
fi

# ---- 2. CLI（debug / release）----
build_one() {  # $1=debug|release → stdout 只输出 bin 路径
  local bin="${2:-}"
  if [ -z "$bin" ]; then
    local flag=""; [ "$1" = release ] && flag="--release"
    log "cargo build $flag --features cli"
    ( cd "$ROOT/spinduck" && cargo build $flag --features cli --bin spinduck ) >&2
    bin="$ROOT/spinduck/target/$1/spinduck"
  fi
  [ -x "$bin" ] || { log "binary not found: $bin"; exit 1; }
  bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"
  log "$1 bin: $bin ($("$bin" version 2>/dev/null | head -1))"
  printf '%s' "$bin"
}
DEBUG_BIN="$(build_one debug "${DEBUG_BIN:-}")"
RELEASE_BIN="$(build_one release "${RELEASE_BIN:-}")"

# ---- 3. compile → 剥离 [[missing]] → deploy byfund 计划 ----
log "compile --analyze (entries=5)"
( cd "$P" && "$DEBUG_BIN" compile --dir . --analyze \
    --entry pkg_split_trade_step3_sh.prc_trd_hz_byfund \
    --entry prc_m0_noop --entry prc_m0_prefix \
    --entry prc_m0_cgetdata --entry prc_m0_cfxj ) > "$WORK/compile.log" 2>&1 || true
for plan in \
  pkg_split_trade_step3_sh.prc_trd_hz_byfund \
  prc_m0_noop prc_m0_prefix prc_m0_cgetdata prc_m0_cfxj; do
  f="$P/.spinduck/plan/$plan.toml"
  [ -f "$f" ] || { log "compile 缺计划 $f"; cat "$WORK/compile.log" >&2; exit 1; }
done
python3 - "$P/.spinduck/plan" <<'PY'
import re,glob,sys
for f in glob.glob(sys.argv[1]+'/*.toml'):
    if 'stripped' in f: continue
    t=open(f).read()
    t=re.sub(r'\[\[missing\]\][^\[]*','',t)
    open(f.replace('.spinduck/plan/','.spinduck/plan/stripped_'),'w').write(t)
PY
BYFUND_PLAN="$P/.spinduck/plan/stripped_pkg_split_trade_step3_sh.prc_trd_hz_byfund.toml"
log "deploy byfund plan → $DB"
rm -f "$DB"
"$DEBUG_BIN" deploy "$BYFUND_PLAN" --db "$DB" > "$WORK/deploy.log" 2>&1 \
  || { log "deploy 失败:"; cat "$WORK/deploy.log" >&2; exit 1; }

# ---- 4. 补齐计划闭包外的表（fnc_get_split_date→dic_all_kind 等，
#        原生 call 的词法缺件门按闭包核对；这些表只读、不进计时主体）----
python3 - "$P" "$DEBUG_BIN" <<'PY'
import sys,subprocess,os
p,bin_=sys.argv[1],sys.argv[2]
extra=['dic_all_kind','swh_all_kind','par_sys_purchase','dat_trd_repurchase',
       'par_sys_marketrate','par_fund_rate_info']
stmts=[]
for t in extra:
    stmts.append(open(f'{p}/sql/ddl/tables/{t}.sql').read())
stmts.append(open(f'{p}/sql/ddl/views/1_V_PAR_FUND_RATE_INFO.sql').read())
for i,s in enumerate(stmts):
    f=f'/tmp/m0_extra_{i}.sql'; open(f,'w').write(s.strip()+';\n')
    r=subprocess.run([bin_,'query','--db',f'{p}/local.duckdb','--sql-file',f,
                      '--out',f+'.json'],capture_output=True,text=True)
    if r.returncode!=0: sys.exit(f"补表 {i} 失败: {r.stderr}{r.stdout}")
print("[m0] 补表完成", file=sys.stderr)
PY

# ---- 5. 种子：参数表 + @DETAIL_N@ 明细 + @QFII_N@ QFII ----
python3 - "$HERE/sql/seed_m0.sql" "$WORK/seed_render.sql" "$DETAIL_N" "$QFII_N" <<'PY'
import sys
s=open(sys.argv[1]).read()
s=s.replace('@DETAIL_N@',sys.argv[3]).replace('@QFII_N@',sys.argv[4])
open(sys.argv[2],'w').write(s)
PY
log "seed detail=$DETAIL_N qfii=$QFII_N"
python3 - "$WORK/seed_render.sql" "$DEBUG_BIN" "$DB" <<'PY'
import sys,subprocess,os
sql=open(sys.argv[1]).read(); bin_,db=sys.argv[2],sys.argv[3]
n=0
for s in sql.split(';'):
    s=s.strip()
    if not s or 'INSERT' not in s.upper(): continue
    f=f'/tmp/m0_seed_{n}.sql'; open(f,'w').write(s+';\n')
    r=subprocess.run([bin_,'query','--db',db,'--sql-file',f,
                      '--out',f+'.json'],capture_output=True,text=True)
    if r.returncode!=0:
        sys.stderr.write(f"seed 语句 {n} 失败: {r.stderr}{r.stdout}\n"); sys.exit(1)
    n+=1
print(f"[m0] seed 语句数={n}",file=sys.stderr)
PY

# 模板库：每轮 cp 一份，call 的写入（dat_fund_cjqs/dat_fund_xw_venture/
# mid_yjqs_detail 更新/qfqscjhbrbb_temp.split_err_msg）不跨轮累积
cp "$DB" "$TEMPLATE_DB"

# ---- 6. 例程源清单（--file 形态：函数文件在前、包文件在后——
#        标量函数宏按注册顺序生成，包内 SQL 引用需先有宏）----
FILES=(
  sql/routines/1_FNC_COM_GET_FUND_CODE.sql
  sql/routines/1_FNC_GET_SPLIT_DATE.sql
  sql/routines/1_FNC_COM_GET_MAIN_XWDM.sql
  sql/routines/1_FNC_GET_STOCK_KIND.sql
  sql/routines/1_FNC_COM_GET_SEAT_FEE_TYPE.sql
  sql/routines/2_FNC_GET_BOND_YJLX.sql
  sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql
  sql/routines/0_SIMPLIFIED_fnc_com_get_between_days.sql
  sql/routines/0_SIMPLIFIED_fnc_com_get_between_jxdays.sql
  sql/routines/0_SIMPLIFIED_fnc_com_getday.sql
  sql/routines/0_SIMPLIFIED_fnc_com_if_deal_date.sql
  sql/routines/0_STUB_prc_com_get_jxdays.sql
  sql/routines/0_STUB_prc_com_get_jxdays_qdii.sql
  sql/stubs/STUB_pkg_split_trade_log.sql
  sql/routines/0_A_FNC_COM_GET_SEAT_PARA.sql
  sql/routines/0_A_FNC_GET_FUND_DATA_MODE.sql
  sql/routines/0_A_FNC_GET_RATE_INFO2.sql
  sql/routines/PKG_SPLIT_TRADE_STEP3_SH.sql
  sql/routines/0_M0_VARIANTS.sql
)
FILE_ARGS=(); for f in "${FILES[@]}"; do FILE_ARGS+=(--file "$P/$f"); done

# ---- 7. 分段计时 ----
# 口径：noop/prefix/cgetdata/cfxj/full 各跑 ROUNDS 轮取中位墙钟（ms，
# perf_counter 端到端，含进程启动——五次调用同一开销，相减时抵消）。
# prefix=prefix−noop；c_get_data=cgetdata−prefix；c_fxj=cfxj−cgetdata；
# qfii=full−cfxj。不为负：max(0,·)。
measure() {  # $1=bin → stdout 五行中位毫秒：noop prefix cgetdata cfxj full
  python3 - "$1" "$STEP_BUDGET" "$ROUNDS" "$CALL_DATE" "$TEMPLATE_DB" \
    "$P" "${FILE_ARGS[@]}" <<'PY'
import subprocess,sys,statistics,time,os,shutil
bin_,budget,rounds,date_,tmpl,p=sys.argv[1:7]
rounds=int(rounds)
file_args=sys.argv[7:]
routines=["prc_m0_noop","prc_m0_prefix","prc_m0_cgetdata","prc_m0_cfxj",
          "pkg_split_trade_step3_sh.prc_trd_hz_byfund"]
acc={r:[] for r in routines}
workdb=os.path.join(p,'round.duckdb')
for r in range(rounds):
    for routine in routines:
        shutil.copyfile(tmpl,workdb)
        cmd=[bin_,'call','--db',workdb,*file_args,
             '--step-budget',budget,routine,date_,'null']
        t=time.perf_counter()
        res=subprocess.run(cmd,capture_output=True,text=True,cwd=p)
        ms=(time.perf_counter()-t)*1000.0
        if res.returncode!=0:
            sys.stderr.write(f"[m0] round{r} {routine} 失败 rc={res.returncode}\n")
            sys.stderr.write(res.stdout[-2000:]+res.stderr[-2000:]+"\n")
            sys.exit(1)
        # 例程内部异常分支也走 OUT p_o_succeed=<msg> 且 rc=0——必须验收干净返回，
        # 否则段差分会把"提前报错"误记成"该段很快"。
        if 'OUT p_o_succeed = 0' not in res.stdout:
            sys.stderr.write(f"[m0] round{r} {routine} 返回异常: {res.stdout.strip()}\n")
            sys.exit(1)
        acc[routine].append(ms)
        print(f"[m0]   round{r} {routine}: {ms:.1f}ms",file=sys.stderr)
for routine in routines:
    print(f"{statistics.median(acc[routine]):.3f}")
PY
}

segments() {  # $1=五行中位数 → prefix cgetdata cfxj qfii（ms，相邻差分）
  python3 - "$1" <<'PY'
import sys
v=[float(x) for x in sys.argv[1].split()]
noop,prefix,cgetdata,cfxj,full=v
def d(a,b): return max(0.0, b-a)
print(f"{d(noop,prefix):.3f} {d(prefix,cgetdata):.3f} {d(cgetdata,cfxj):.3f} {d(cfxj,full):.3f}")
# 同时把五个原始中位数带出去供 JSON 复核
print(f"#raw {noop:.3f} {prefix:.3f} {cgetdata:.3f} {cfxj:.3f} {full:.3f}",file=sys.stderr)
PY
}

log "measure debug ($ROUNDS rounds × 5 calls)"
DEBUG_MS="$(measure "$DEBUG_BIN")"
log "measure release ($ROUNDS rounds × 5 calls)"
RELEASE_MS="$(measure "$RELEASE_BIN")"

DEBUG_SEG="$(segments "$DEBUG_MS")"
RELEASE_SEG="$(segments "$RELEASE_MS")"

HEAD="$(git -C "$ROOT" rev-parse HEAD)"
python3 - "$ROOT" "$HEAD" "$DEBUG_SEG" "$RELEASE_SEG" "$DEBUG_MS" "$RELEASE_MS" \
  "$DETAIL_N" "$QFII_N" "$ROUNDS" <<'PY'
import sys,json,os
root,head,dseg,rseg,dms,rms,dn,qn,rounds=sys.argv[1:10]
ds=[float(x) for x in dseg.split()]; rs=[float(x) for x in rseg.split()]
doc={
 "routine":"prc_trd_hz_byfund",
 "git_head":head,
 "unit":"ms",
 "method":"truncated-variant end-to-end wall clock per segment; "
         "prefix=prc_m0_prefix-prc_m0_noop, c_get_data=prc_m0_cgetdata-prc_m0_prefix, "
         "c_fxj=prc_m0_cfxj-prc_m0_cgetdata, qfii=full-prc_m0_cfxj; "
         "median of %s rounds; seed detail=%s qfii=%s"%(rounds,dn,qn),
 "builds":{
  "debug":{"prefix_ms":ds[0],"c_get_data_ms":ds[1],"c_fxj_ms":ds[2],"qfii_ms":ds[3]},
  "release":{"prefix_ms":rs[0],"c_get_data_ms":rs[1],"c_fxj_ms":rs[2],"qfii_ms":rs[3]},
 },
}
out=os.path.join(root,'docs','goals','artifacts')
os.makedirs(out,exist_ok=True)
path=os.path.join(out,'set-oriented-v3-m0-profile.json')
open(path,'w').write(json.dumps(doc,ensure_ascii=False,indent=2)+"\n")
print(path)
print(json.dumps(doc,ensure_ascii=False,indent=2))
PY
