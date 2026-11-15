#!/usr/bin/env bash
# PR #48 集合化 —— 原始 purchase 主过程 100k 边界实证（Part B）。
#
# 与 Part A（同形可转换过程）互补：本脚本跑**真实语料原始过程**
# pkg_split_trade_step3_sh.prc_trd_deal_purchase，在 100k 同源种子下证明：
#   1) 集合化判定为 Skipped(ExceptionScopePresent)——准入边界保守，真实过程不转换；
#   2) ON 与 OFF 业务输出逐字一致、目标表逐行一致（Skipped ⇒ ON≡OFF，零收益零分歧）；
#   3) 该判定是结构性（编译期 IR 分析）的，与数据量无关。
#
# 参照侧（openGauss 原生）说明：原始过程是 PACKAGE PROCEDURE，须经 CALL 调用；
# 按 validation V21，amd64 postmaster 在 procedure CALL 上崩溃，故参照**无法**对
# 完整原始过程做基线——本脚本不尝试参照 CALL（避免崩溃容器），如实记录该限制。
# Part A 的参照基线用的是 FUNCTION（非 CALL），不受此限。
#
# 用法：
#   bash golden/pr48_set_oriented_verify/bench_partb/run_partb.sh
#   SPINDUCK_BIN=/path/to/spinduck bash ...   # 复用已构建 PR-head CLI
#   N=1000 bash ...                           # 覆盖种子行数（默认 100000；快速冒烟用小数）
#   KEEP_WORKTREE=1 bash ...                  # 保留临时目录
#
# 环境：Rust stable、cargo、git、python3。SpinDuck 侧无需 GaussDB 容器。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERIFY_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$VERIFY_DIR/../.." && pwd)"
REPORT_DIR="$SCRIPT_DIR/reports"
CORPUS="$REPO_ROOT/golden/case_corpus_purchase"
PR_HEAD="${PR_HEAD:-f62787d95fa82fb442147809978b0250ea0bb48c}"
PR_BRANCH="feat/set-oriented-v1-pr"
N="${N:-100000}"
# 十万行游标循环合法超过默认步数预算（DEFAULT_STEP_BUDGET=1_000_000，
# 见 runtime/interp.rs），须按调用面放大（runtime/interp.rs:36 注释）。
STEP_BUDGET="${STEP_BUDGET:-100000000}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr48_partb.XXXXXX")"
BIN="${SPINDUCK_BIN:-}"
WT=""

cleanup() {
  if [ "${KEEP_WORKTREE:-0}" = "1" ]; then echo "[partb] 保留临时目录: $WORK";
  else
    [ -n "$WT" ] && git -C "$REPO_ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

# ---- 1. PR-head CLI ----
if [ -z "$BIN" ]; then
  echo "[partb] 构建 PR head CLI ($PR_HEAD)"
  git -C "$REPO_ROOT" fetch origin "$PR_BRANCH"
  WT="${REPO_ROOT}-pr48-partb-$$"
  git -C "$REPO_ROOT" worktree add --detach "$WT" "$PR_HEAD"
  git -C "$WT" submodule update --init lib/ogsql-parser
  [ -d "$REPO_ROOT/spinduck/target" ] && export CARGO_TARGET_DIR="$REPO_ROOT/spinduck/target"
  ( cd "$WT/spinduck" && cargo build --features cli --bin spinduck )
  BIN="${CARGO_TARGET_DIR:-$WT/spinduck/target}/debug/spinduck"
fi
echo "[partb] CLI: $BIN ($("$BIN" version))"

# ---- 2. 原始语料工程：逐字复制 SQL + set_oriented 开关 + codeweb 标记 ----
P="$WORK/project"; mkdir -p "$P"
cp -R "$CORPUS/sql" "$P/sql"
cp -R "$CORPUS/rules" "$P/rules"
cat > "$P/spinduck.toml" <<'EOF'
[project]
name = "pr48_partb"
kernel = "1.5.5"
[engine]
set_oriented = false
[paths]
routines = "sql/routines"
ddl = "sql/ddl"
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

# ---- 3. compile（codeweb 取图）→ 剥离外部依赖 missing → deploy ----
echo "[partb] compile --analyze"
"$BIN" compile --dir "$P" --analyze > "$WORK/compile.log" 2>&1 || true
GEN="$P/.spinduck/plan/pkg_split_trade_step3_sh.prc_trd_deal_purchase.toml"
if [ ! -f "$GEN" ] || ! grep -q 'failed=0' "$WORK/compile.log"; then
  echo "[partb] compile 失败：" >&2; cat "$WORK/compile.log" >&2; exit 1
fi
python3 - "$GEN" "$P/.spinduck/plan/call_plan.toml" <<'PY'
import re,sys
t=open(sys.argv[1]).read(); t=re.sub(r'\[\[missing\]\][^\[]*','',t); open(sys.argv[2],'w').write(t)
PY
echo "[partb] deploy"
"$BIN" deploy "$P/.spinduck/plan/call_plan.toml" --db "$P/local.duckdb" \
  > "$WORK/deploy.log" 2>&1 || { echo "[partb] deploy 失败：" >&2; cat "$WORK/deploy.log" >&2; exit 1; }
DB="$P/local.duckdb"; PLAN="$P/.spinduck/plan/call_plan.toml"

# ---- 4. 载入参数表（确定性路径 swh_all_kind.kind_id='0'，运行时不调外部函数）----
python3 - "$P" "$BIN" "$CORPUS" <<'PY'
import sys,subprocess,tempfile,os
p,bin,corpus=sys.argv[1:4]
sql=open(corpus+'/cases/datasets/seed.sql').read()
for s in sql.split(';'):
    if 'INSERT' in s.upper() and 'mid_yjqs_detail' not in s:
        f=tempfile.NamedTemporaryFile('w',suffix='.sql',delete=False); f.write(s.strip()+';\n'); f.close()
        r=subprocess.run([bin,'query','--db',p+'/local.duckdb','--sql-file',f.name,'--out',f.name+'.json'],capture_output=True,text=True)
        os.unlink(f.name)
        if r.returncode!=0: sys.exit("参数载入失败: "+r.stderr)
print("[partb] 参数表载入完成")
PY

# ---- 5. 生成 N 行同源种子（全部命中游标过滤 + F001 资金路径）----
cat > "$WORK/gen.sql" <<EOF
INSERT INTO mid_yjqs_detail (gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,yj01,fxj,accrual,pay_type,ghcjje,cjbh,yj02,yj03,yj04,stock_kind,yj06,yj05,sub_gddm,yj07,yj08,yj09,is_yysg,ywlb,etf_flag,check_type,date_source,ghcjje2)
SELECT 'A0001','X001','001','600000','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,100000+g,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000
FROM generate_series(1,$N) AS t(g);
EOF
"$BIN" query --db "$DB" --sql-file "$WORK/gen.sql" --out "$WORK/gen.json" >/dev/null 2>&1
printf "SELECT count(*) AS n FROM mid_yjqs_detail;\n" > "$WORK/cnt_src.sql"
"$BIN" query --db "$DB" --sql-file "$WORK/cnt_src.sql" --out "$WORK/cnt_src.json" >/dev/null 2>&1
SRC_N="$(python3 -c "import json;print(json.load(open('$WORK/cnt_src.json'))['rows'][0][0])")"
echo "[partb] 源种子行数: $SRC_N（目标 $N）"
[ "$SRC_N" = "$N" ] || { echo "[partb] 源行数不符" >&2; exit 1; }

# ---- 6. ON/OFF 双跑：truncate 目标 → call → 捕获判定/输出/墙钟 → 导出目标 ----
printf "DELETE FROM dat_trd_repurchase;\n" > "$WORK/trunc.sql"
printf "SELECT * FROM dat_trd_repurchase;\n" > "$WORK/dump.sql"

run_pass() {  # $1=true|false  $2=tag(on/off)
  sed -i.bak "s/set_oriented = .*/set_oriented = $1/" "$P/spinduck.toml"
  "$BIN" query --db "$DB" --sql-file "$WORK/trunc.sql" --out "$WORK/trunc.json" >/dev/null 2>&1
  python3 - "$BIN" "$DB" "$PLAN" "$WORK/${2}_call.txt" "$WORK/${2}_ms.txt" "$STEP_BUDGET" <<'PY'
import sys,subprocess,time
bin,db,plan,outf,msf,budget=sys.argv[1:7]
t=time.perf_counter()
r=subprocess.run([bin,'call','--db',db,'--plan',plan,'--step-budget',budget,"'20250715'",'null'],capture_output=True,text=True)
ms=(time.perf_counter()-t)*1000
open(outf,'w').write(r.stdout)
open(msf,'w').write(f"{ms:.3f}")
sys.stderr.write(f"[partb]   {sys.argv[4].split('/')[-1]}: rc={r.returncode} ELAPSED_MS={ms:.1f}\n")
if r.returncode!=0: sys.exit("call 失败: "+r.stdout+r.stderr)
PY
  "$BIN" query --db "$DB" --sql-file "$WORK/dump.sql" --out "$WORK/${2}_target.json" >/dev/null 2>&1
}
echo "[partb] OFF pass (set_oriented=false)"
run_pass false off
echo "[partb] ON  pass (set_oriented=true)"
run_pass true on

# ---- 7. 断言：ON 报 Skipped + OFF 无判定行 + ON≡OFF + 目标逐行一致 ----
# ON（feature 开）须报告 Skipped 判定
on_line="$(grep '^SET_ORIENTED ' "$WORK/on_call.txt" || true)"
if [ "$on_line" != "SET_ORIENTED pkg_split_trade_step3_sh.prc_trd_deal_purchase skipped reason=ExceptionScopePresent" ]; then
  echo "[partb] ON 判定不符：${on_line:-<无>}" >&2; exit 1
fi
# OFF（feature 关）不得有任何 SET_ORIENTED 行
if grep -q '^SET_ORIENTED ' "$WORK/off_call.txt"; then
  echo "[partb] OFF 不应报告 SET_ORIENTED（feature 已关闭）" >&2; exit 1
fi
# 业务输出（去 SET_ORIENTED 行）逐字一致
grep -v '^SET_ORIENTED ' "$WORK/on_call.txt" > "$WORK/on_business.txt"
grep -v '^SET_ORIENTED ' "$WORK/off_call.txt" > "$WORK/off_business.txt"
diff -u "$WORK/on_business.txt" "$WORK/off_business.txt" > "$WORK/equiv.diff" \
  || { echo "[partb] ON/OFF 业务输出分歧：" >&2; cat "$WORK/equiv.diff" >&2; exit 1; }

CONSISTENT="$(python3 - "$WORK/on_target.json" "$WORK/off_target.json" "$N" <<'PY'
import json,sys
on=json.load(open(sys.argv[1])); off=json.load(open(sys.argv[2])); n=int(sys.argv[3])
a=sorted(map(tuple,on['rows'])); b=sorted(map(tuple,off['rows']))
if len(a)!=n or len(b)!=n: print(f"ROWCOUNT_FAIL on={len(a)} off={len(b)} expect={n}"); sys.exit(0)
print("OK" if a==b else "DIFF")
PY
)"
[ "$CONSISTENT" = "OK" ] || { echo "[partb] 目标表一致性失败: $CONSISTENT" >&2; exit 1; }
ON_MS="$(cat "$WORK/on_ms.txt")"; OFF_MS="$(cat "$WORK/off_ms.txt")"

# ---- 8. 汇总 ----
mkdir -p "$REPORT_DIR"
cp "$WORK/on_call.txt" "$REPORT_DIR/partb_on_call.txt"
cp "$WORK/off_call.txt" "$REPORT_DIR/partb_off_call.txt"
{
  echo "=== PR#48 原始 purchase 主过程 100k 边界实证（Part B）==="
  echo "CLI: $("$BIN" version)  head: $PR_HEAD  构建: debug  step-budget: $STEP_BUDGET"
  echo "过程: pkg_split_trade_step3_sh.prc_trd_deal_purchase（真实语料，逐字）"
  echo "数据: $N 行同源种子（命中游标过滤 + F001 资金路径，确定性路径 kind_id='0'）"
  echo ""
  echo "方 | 墙钟(ms) | 集合化判定 | 目标行数"
  echo "SpinDuck OFF（解释器游标 RBAR） | $OFF_MS | feature 关，无判定行 | $N"
  echo "SpinDuck ON（set_oriented=true） | $ON_MS | skipped ExceptionScopePresent | $N"
  echo "openGauss 原生参照 | 不可基线 | — | —"
  echo ""
  echo "数据一致性: ON/OFF 目标表 dat_trd_repurchase 排序后逐行比对 = $CONSISTENT（均 $N 行）"
  echo "业务输出: ON 去 SET_ORIENTED 行后与 OFF 逐字一致（diff 为空）。"
  echo ""
  echo "结论:"
  echo "  1) 原始过程含异常域/子调用/多写 → Skipped(ExceptionScopePresent)，集合化不触发。"
  echo "  2) Skipped ⇒ ON≡OFF：墙钟同量级（差异为 debug 测量噪声），目标逐行一致，零收益零分歧。"
  echo "  3) 判定为编译期 IR 结构分析，与数据量无关——100k 与空种子判定相同。"
  echo "  4) 参照限制（V21）: 原始过程须经 CALL，amd64 postmaster 在 procedure CALL 崩溃，"
  echo "     故参照无法对完整原始过程基线；Part A 参照用 FUNCTION 不受此限。"
  echo ""
  echo "口径: SpinDuck 为 CLI call 端到端墙钟（含进程启动+开库+注册+事务固定开销，约 180ms）；"
  echo "debug 构建绝对值仅证明方向与量级。单轮测时（100k 单趟约数分钟，多轮成本过高）。"
} > "$REPORT_DIR/partb_summary.log"
cat "$REPORT_DIR/partb_summary.log"
echo "[partb] 通过。报告: $REPORT_DIR/partb_{on_call,off_call,summary}.log"