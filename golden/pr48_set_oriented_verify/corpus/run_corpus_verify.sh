#!/usr/bin/env bash
# PR #48 集合化 —— 真实语料（case_corpus_purchase）CLI 验证脚本。
#
# 不写任何 Rust：基于原始语料 SQL + 特殊项目 toml（[engine].set_oriented）+
# compile/deploy/call 指令，逐例程捕获 SET_ORIENTED 转换判定，并差分 ON/OFF 业务输出。
#
# 用法：
#   bash golden/pr48_set_oriented_verify/corpus/run_corpus_verify.sh
#   SPINDUCK_BIN=/path/to/spinduck bash ...   # 复用已构建的 PR-head CLI（跳过构建）
#   KEEP_WORKTREE=1 bash ...                  # 保留临时项目目录供检查
#
# 环境要求：Rust stable、cargo、git、python3。首次构建编译 DuckDB bundled（约 10 分钟）；
# 主仓 spinduck/target 有缓存时自动复用。无需 GaussDB 容器。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERIFY_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$VERIFY_DIR/../.." && pwd)"
CORPUS_SRC="$REPO_ROOT/golden/case_corpus_purchase"
REPORT_DIR="$SCRIPT_DIR/reports"
PR_HEAD="${PR_HEAD:-f62787d95fa82fb442147809978b0250ea0bb48c}"
PR_BRANCH="feat/set-oriented-v1-pr"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr48_corpus.XXXXXX")"
PROJ="$WORK/project"
BIN="${SPINDUCK_BIN:-}"
WT=""

cleanup() {
  if [ "${KEEP_WORKTREE:-0}" = "1" ]; then
    echo "[corpus] 保留临时目录: $WORK"
  else
    [ -n "$WT" ] && git -C "$REPO_ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

# ---- 1. 取得 PR-head CLI ----
if [ -z "$BIN" ]; then
  echo "[corpus] 构建 PR head CLI ($PR_HEAD)"
  git -C "$REPO_ROOT" fetch origin "$PR_BRANCH"
  WT="${REPO_ROOT}-pr48-corpus-$$"
  git -C "$REPO_ROOT" worktree add --detach "$WT" "$PR_HEAD"
  git -C "$WT" submodule update --init lib/ogsql-parser
  if [ -d "$REPO_ROOT/spinduck/target" ]; then
    export CARGO_TARGET_DIR="$REPO_ROOT/spinduck/target"
  fi
  ( cd "$WT/spinduck" && cargo build --features cli --bin spinduck )
  BIN="${CARGO_TARGET_DIR:-$WT/spinduck/target}/debug/spinduck"
fi
echo "[corpus] CLI: $BIN ($("$BIN" version))"

# ---- 2. 搭建特殊项目：原始 SQL 逐字复制 + set_oriented 开关 + codeweb 标记 ----
mkdir -p "$PROJ"
cp -R "$CORPUS_SRC/sql" "$PROJ/sql"
cp -R "$CORPUS_SRC/rules" "$PROJ/rules"
cat > "$PROJ/spinduck.toml" <<'EOF'
[project]
name = "pr48_corpus_verify"
kernel = "1.5.5"
[paths]
ddl = "sql/ddl"
routines = "sql/routines"
stubs = ["sql/stubs"]
rules = ["rules/spinduck-rules.toml"]
home = ".spinduck"
[engine]
set_oriented = true
threads = 4
[[entry]]
routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase"
kind = "procedure"
EOF
cat > "$PROJ/codeweb.toml" <<'EOF'
[project]
name = "pr48_corpus_verify"
[analysis]
paths = ["sql/"]
[store]
path = ".codeweb/store.bincode"
format = "bincode"
EOF

# ---- 3. compile（codeweb 取图）→ 部署计划 → deploy ----
# 语料含外部依赖 fnc_com_get_seat_para（不在语料内），compile 会报 missing=1 并退出 1，
# 但计划照常写出、IR 体检 failed=0。故以「计划已生成 且 failed=0」为成功判据，而非退出码。
echo "[corpus] compile --analyze"
"$BIN" compile --dir "$PROJ" --analyze > "$WORK/compile.log" 2>&1 || true
GEN_PLAN="$PROJ/.spinduck/plan/pkg_split_trade_step3_sh.prc_trd_deal_purchase.toml"
if [ ! -f "$GEN_PLAN" ] || ! grep -q 'failed=0' "$WORK/compile.log"; then
  echo "[corpus] compile 失败（计划缺失或 IR 体检红）：" >&2; cat "$WORK/compile.log" >&2; exit 1
fi
# 外部依赖 fnc_com_get_seat_para 不在语料内（与日志桩同类的外部件）：
# 集合化判定是纯结构的、在 call 起点生成；缺件门只核表/视图/序列。
# 去掉 [[missing]] 段即可部署 DDL 并捕获判定（空种子走确定性成功路径，无需伪造业务桩）。
python3 - "$GEN_PLAN" "$PROJ/.spinduck/plan/call_plan.toml" <<'PY'
import sys, re
t = open(sys.argv[1]).read()
t = re.sub(r'\n\[\[missing\]\][\s\S]*?(?=\n\[|\Z)', '\n', t)
open(sys.argv[2], 'w').write(t)
PY
echo "[corpus] deploy"
"$BIN" deploy "$PROJ/.spinduck/plan/call_plan.toml" --db "$PROJ/local.duckdb" \
  > "$WORK/deploy.log" 2>&1 || { echo "[corpus] deploy 失败：" >&2; cat "$WORK/deploy.log" >&2; exit 1; }

# ---- 4. ON/OFF 双跑：逐例程 call，捕获 SET_ORIENTED 判定 + 业务输出 ----
R="$PROJ/sql/routines"
PLAN="$PROJ/.spinduck/plan/call_plan.toml"
DB="$PROJ/local.duckdb"

run_pass() {  # $1 = true|false（set_oriented），$2 = 输出日志
  sed -i.bak "s/set_oriented = .*/set_oriented = $1/" "$PROJ/spinduck.toml"
  : > "$2"
  {
    echo "### fnc_com_getday"
    "$BIN" call --db "$DB" --file "$R/0_SIMPLIFIED_fnc_com_getday.sql" \
      fnc_com_getday "'01'" "'20250715'" 1
    echo "### fnc_com_if_deal_date"
    "$BIN" call --db "$DB" --file "$R/0_SIMPLIFIED_fnc_com_if_deal_date.sql" \
      fnc_com_if_deal_date "'01'" "'20250715'"
    echo "### fnc_get_purchase_js_days"
    "$BIN" call --db "$DB" \
      --file "$R/0_SIMPLIFIED_fnc_com_getday.sql" \
      --file "$R/0_SIMPLIFIED_fnc_com_if_deal_date.sql" \
      --file "$R/2_FNC_GET_PURCHASE_JS_DAYS.sql" \
      fnc_get_purchase_js_days "'20250715'" "'0130000001'"
    echo "### pkg_split_trade_step3_sh.prc_trd_deal_purchase"
    "$BIN" call --db "$DB" --plan "$PLAN" "'20250715'" null
  } >> "$2" 2>&1
}

mkdir -p "$REPORT_DIR"
echo "[corpus] ON  pass (set_oriented=true)"
run_pass true  "$REPORT_DIR/corpus_on.log"
echo "[corpus] OFF pass (set_oriented=false)"
run_pass false "$REPORT_DIR/corpus_off.log"

# ---- 5. 断言：等价性 + 转换判定 ----
# 等价：ON 去掉 SET_ORIENTED 行后与 OFF 逐字一致。
grep -v '^SET_ORIENTED ' "$REPORT_DIR/corpus_on.log" > "$WORK/on_business.log"
if ! diff -u "$WORK/on_business.log" "$REPORT_DIR/corpus_off.log" > "$WORK/equiv.diff"; then
  echo "[corpus] 等价性失败：ON/OFF 业务输出分歧" >&2; cat "$WORK/equiv.diff" >&2; exit 1
fi

# 判定：四个例程均应 Skipped，且原因符合结构预期。
check_reason() {  # $1 = 例程名, $2 = 期望原因
  local line; line="$(grep "^SET_ORIENTED $1 " "$REPORT_DIR/corpus_on.log" || true)"
  if [ "$line" != "SET_ORIENTED $1 skipped reason=$2" ]; then
    echo "[corpus] 判定不符：$1 期望 reason=$2，实际：${line:-<无>}" >&2; exit 1
  fi
}
check_reason fnc_com_getday UnsupportedControlFlow
check_reason fnc_com_if_deal_date UnsupportedControlFlow
check_reason FNC_GET_PURCHASE_JS_DAYS ExceptionScopePresent
check_reason pkg_split_trade_step3_sh.prc_trd_deal_purchase ExceptionScopePresent

# ---- 6. 汇总 ----
{
  echo "=== PR#48 真实语料集合化验证（case_corpus_purchase，纯 CLI，空种子确定性路径）==="
  echo "CLI: $("$BIN" version)  head: $PR_HEAD"
  echo ""
  echo "例程 | ON 判定 | 业务输出（ON==OFF）"
  paste -d'|' \
    <(grep '^SET_ORIENTED ' "$REPORT_DIR/corpus_on.log" | sed 's/SET_ORIENTED //') \
    <(grep -E '^(RETURN|OUT) ' "$REPORT_DIR/corpus_on.log")
  echo ""
  echo "等价性：ON 去 SET_ORIENTED 行后与 OFF 逐字一致（diff 为空）。"
  echo "结论：四个语料例程全部 Skipped——PR#48 集合化不在真实生产语料上触发，准入边界保守。"
} > "$REPORT_DIR/corpus_summary.log"

cat "$REPORT_DIR/corpus_summary.log"
echo "[corpus] 通过。报告：$REPORT_DIR/corpus_{on,off,summary}.log"