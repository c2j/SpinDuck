#!/usr/bin/env bash
# 论文探针：把论文案例与开源基准的例程逐个跑 Off 与集合化（batch-atomic / strict），
# 记录集合化判定、「Off ↔ 集合化」是否逐值相等、Off 与参照 SQL 是否一致，产出差距矩阵。
#
# 两个独立工程（各自一份 spinduck.toml、DDL、例程、probes.tsv）：
#   main  上一级目录本体：TPC-H / TPC-DS 最小列子集 + 定标阶梯 + 各论文的合成案例
#   tpch  tpch/ 子目录：TPC-H 形状的确定性数据，承载基于 TPC-H 的论文案例
#
# 语义合同（与 golden/case_* 的 GaussDB 黄金层不同，本包没有 GaussDB 参照侧）：
#   1. 等价性基准 = SpinDuck Off（解释器逐行）自身的输出——只回答「集合化是否与解释器一致」，
#      不回答「解释器是否与 GaussDB 一致」（后者由 case_* 黄金层负责）。
#   2. 参照列（off_vs_oracle）是另一回事：cases/oracle.sql 里的 SQL 在同一个 DuckDB 上跑出的结果，
#      用来检验 Off 的结果本身对不对。参照 SQL 分「论文给的」和「本包手写的」两种来源，文件里有标注。
#   3. 期望列（off_vs_expect）是清单里手写的期望值（用于数值语义这类无法用 SQL 参照的探针）。
#   4. 判定面 = call 的 SET_ORIENTED 行（运行期）与 compile --admit 的 ADMIT 行（编译期）。
#   5. Skipped 不是失败也不是通过：矩阵里「判定」与「等价」两列分开看。
#
# 产物：
#   reports/matrix.tsv            差距矩阵（入库）
#   reports/summary.txt           汇总数字（入库）；报告引用这里，避免与矩阵脱节
#   reports/admit_<工程>_<模式>.txt   compile --admit 判定（入库）
#   reports/compile_<工程>.log、compile_fail.txt、run_env.txt（入库）
#   reports/sqlprocbench_compile.txt   基准整体解析体检（设了 SQLPROCBENCH_SRC 才生成）
#   reports/evidence/<工程>/<id>.txt   入库证据：集合化成功、Off 出错、与参照不一致、与期望不符、query 通道
#   reports/work/<工程>/<id>.txt       全量明细（.gitignore，每次运行重建）
#
# 用法：
#   bash golden/paper_set_oriented_probes/run_probes.sh
#   SPINDUCK_BIN=/path/to/spinduck bash ...   # 缺省 spinduck/target/debug/spinduck，必须已构建
#   SQLPROCBENCH_SRC=<SQL-ProcBench>/src/PLSQL bash ...   # 追加：基准整体解析体检
#   KEEP_WORK=1 bash ...                      # 保留临时工程目录（含 duckdb）
#
# 环境：bash 3.2+、python3、rsync；不需要容器，不访问网络。约 3 分钟。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BIN="${SPINDUCK_BIN:-$REPO_ROOT/spinduck/target/debug/spinduck}"
REPORTS="$SCRIPT_DIR/reports"
EVIDENCE="$REPORTS/evidence"
[ -x "$BIN" ] || { echo "[probe] 找不到 CLI：$BIN（先 cargo build --features cli，或用 SPINDUCK_BIN 指定）" >&2; exit 1; }
mkdir -p "$EVIDENCE"
rm -rf "$EVIDENCE"/*
rm -f "$REPORTS"/admit_*.txt "$REPORTS"/compile_*.log   # 上一次运行的产物；本次会重新生成
rm -f "$REPORTS"/compile_fail.txt "$REPORTS"/duckdb_mechanisms.txt "$REPORTS"/matrix.tsv "$REPORTS"/summary.txt
WORK="$(mktemp -d "${TMPDIR:-/tmp}/paper_probe.XXXXXX")"

cleanup() {
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "[probe] 保留临时目录: $WORK";
  else rm -rf "$WORK"; fi
}
trap cleanup EXIT

# 工程：名字:相对路径。tpch 缺目录时跳过。
PROJECTS="main:."
[ -d "$SCRIPT_DIR/tpch" ] && PROJECTS="$PROJECTS tpch:tpch"
PROJECT_NAMES=""
for spec in $PROJECTS; do PROJECT_NAMES="$PROJECT_NAMES ${spec%%:*}"; done

echo "[probe] CLI: $BIN ($("$BIN" version))"
{
  echo "spinduck=$("$BIN" version)"
  echo "git_head=$(git -C "$REPO_ROOT" rev-parse HEAD)"
  echo "git_dirty_src=$(git -C "$REPO_ROOT" status --porcelain -- spinduck/src | wc -l | tr -d ' ')"
  echo "date=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "projects=$(echo $PROJECT_NAMES | tr ' ' ',')"
} > "$REPORTS/run_env.txt"

run_sql_file() {  # $1=工程目录 $2=db $3=sql 文件；按 ; 切分逐条经 query 执行
  python3 - "$BIN" "$1" "$2" "$3" "$WORK" <<'PY'
import os, subprocess, sys
bin_, P, db, path, work = sys.argv[1:6]
text = open(os.path.join(P, path), encoding="utf-8").read()
body = "\n".join(l for l in text.splitlines() if not l.strip().startswith("--"))
stmts = [s.strip() for s in body.split(";") if s.strip()]
tag = os.path.splitext(os.path.basename(path))[0]
d = os.path.join(work, "stmts_" + tag)
os.makedirs(d, exist_ok=True)
for i, s in enumerate(stmts):
    f = os.path.join(d, f"s{i:03d}.sql")
    open(f, "w", encoding="utf-8").write(s + ";\n")
    r = subprocess.run([bin_, "query", "--db", db, "--sql-file", f, "--out", f + ".json"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        print(f"FAIL {path} stmt {i}: {s[:160]!r}\n{(r.stdout + r.stderr).strip()[:600]}", file=sys.stderr)
        sys.exit(1)
print(f"{len(stmts)}")
PY
}

# ---- （可选）SQL-ProcBench PL/SQL 语料整体解析体检 ----
# 设 SQLPROCBENCH_SRC=<仓库>/src/PLSQL 才执行：https://github.com/microsoft/SQL-ProcBench
# 的 zip 解包路径。只做解析层体检（spinduck compile），不执行。产物入库。
if [ -n "${SQLPROCBENCH_SRC:-}" ]; then
  [ -d "$SQLPROCBENCH_SRC" ] || { echo "[probe] SQLPROCBENCH_SRC 不是目录: $SQLPROCBENCH_SRC" >&2; exit 1; }
  {
    echo "# spinduck compile <SQL-ProcBench>/src/PLSQL —— 解析层体检（不执行）"
    echo "# 源: https://github.com/microsoft/SQL-ProcBench （main，$(date -u +%Y-%m-%d)）；CLI: $("$BIN" version)"
    "$BIN" compile "$SQLPROCBENCH_SRC" 2>&1 | sed -E "s#^((OK|FAIL) +)$SQLPROCBENCH_SRC/#\\1#" || true
  } > "$REPORTS/sqlprocbench_compile.txt"
  echo "[probe] SQL-ProcBench: $(grep -E '^summary:' "$REPORTS/sqlprocbench_compile.txt")"
fi

for spec in $PROJECTS; do
  NAME="${spec%%:*}"
  REL="${spec##*:}"
  SRC="$SCRIPT_DIR/$REL"

  # ---- 1. 临时工程：拷贝探针工程，运行态（.spinduck / .codeweb / duckdb）只落在临时目录 ----
  P="$WORK/$NAME"
  mkdir -p "$P"
  rsync -a --exclude .spinduck --exclude .codeweb --exclude reports "$SRC/" "$P/"
  DB="$P/local.duckdb"
  MANIFEST="$SRC/probes.tsv"
  mkdir -p "$REPORTS/evidence/$NAME" "$REPORTS/work/$NAME"
  rm -f "$REPORTS/evidence/$NAME"/*.txt "$REPORTS/work/$NAME"/*.txt

  call_routines() {  # 所有 kind=call / call-ordered 的例程（去重，保持清单顺序）
    awk -F'\t' '($2=="call" || $2=="call-ordered") && !seen[$3]++ {print $3}' "$MANIFEST"
  }
  # 不用 mapfile：macOS 自带 bash 是 3.2。
  ROUTINES=()
  while IFS= read -r r; do ROUTINES+=("$r"); done < <(call_routines)
  [ "${#ROUTINES[@]}" -gt 0 ] || { echo "[probe] $NAME: probes.tsv 没有 call 探针" >&2; exit 1; }

  # ---- 2. 项目态编译：按探针清单逐个入口落盘运行计划 ----
  #    不因为个别入口体检失败而中止：失败本身就是探针要记录的结论（矩阵 compile 列）。
  ENTRY_ARGS=()
  for r in "${ROUTINES[@]}"; do ENTRY_ARGS+=(--entry "$r"); done
  ( cd "$P" && "$BIN" compile --dir . --analyze "${ENTRY_ARGS[@]}" ) > "$WORK/compile_$NAME.log" 2>&1 || true
  cp "$WORK/compile_$NAME.log" "$REPORTS/compile_$NAME.log"

  # ---- 3. deploy：计划的 ddl_tables 只含该入口闭包引用到的 DDL 文件，按 DDL 文件
  #    集合去重后逐个部署，保证所有表都建过。全部在灌数据之前完成（幂等重建、表为空）。 ----
  DEPLOYED=0
  SEEN_DDL="|"
  for r in "${ROUTINES[@]}"; do
    PLAN="$P/.spinduck/plan/$r.toml"
    [ -f "$PLAN" ] || continue
    grep -q '^\[\[missing\]\]' "$PLAN" && continue
    DDL="$(grep -m1 '^ddl_tables' "$PLAN")"
    case "$SEEN_DDL" in *"|$DDL|"*) continue ;; esac
    SEEN_DDL="$SEEN_DDL$DDL|"
    ( cd "$P" && "$BIN" deploy ".spinduck/plan/$r.toml" --db local.duckdb ) > "$WORK/deploy.log" 2>&1 \
      || { echo "[probe] deploy 跳过（该入口体检失败，计划不可部署）: $NAME/$r" >&2; continue; }
    DEPLOYED=$((DEPLOYED + 1))
  done
  [ "$DEPLOYED" -gt 0 ] || { echo "[probe] $NAME: 没有可部署的计划（全部入口体检失败）" >&2; exit 1; }

  # ---- 4. 数据与参照：cases/*.sql 按「种子/生成器 → 其余（参照表）」的顺序执行 ----
  SEED_N=0
  for f in seed.sql gen.sql; do
    [ -f "$P/cases/$f" ] || continue
    n="$(run_sql_file "$P" "$DB" "cases/$f")"
    SEED_N=$((SEED_N + n))
  done
  for f in $(cd "$P/cases" && ls *.sql 2>/dev/null | grep -vE '^(seed|gen)\.sql$' | sort); do
    n="$(run_sql_file "$P" "$DB" "cases/$f")"
    SEED_N=$((SEED_N + n))
  done

  # ---- 5. 编译期判定（L1/L2 + 无绑定守卫），两种模式各一份 ----
  for mode in batch-atomic strict; do
    ( cd "$P" && "$BIN" compile --dir . --analyze "${ENTRY_ARGS[@]}" \
        --admit=report --db local.duckdb --set-oriented="$mode" ) \
        > "$WORK/admit_${NAME}_$mode.log" 2>&1 || true
    grep -E '^admit:|^ADMIT' "$WORK/admit_${NAME}_$mode.log" > "$REPORTS/admit_${NAME}_$mode.txt" || true
  done
  echo "[probe] [$NAME] compile: $(grep -E '^compile:' "$WORK/compile_$NAME.log" | tail -1)"
  echo "[probe] [$NAME] deploy + data: $SEED_N 条语句；admit: $(head -1 "$REPORTS/admit_${NAME}_batch-atomic.txt")"

  # ---- 6. 逐探针：Off → batch-atomic → strict，各自落结果快照并比对 ----
  python3 - "$BIN" "$P" "$DB" "$WORK" "$REPORTS" "$MANIFEST" "$NAME" "$WORK/compile_$NAME.log" <<'PY'
import json, math, os, re, shlex, subprocess, sys

bin_, P, db, work, reports, manifest, proj, compile_log = sys.argv[1:9]
workdir = os.path.join(reports, "work", proj)
evidence = os.path.join(reports, "evidence", proj)
qseq = [0]

# 例程 → 源文件（create or replace procedure|function <name>），用于把项目态编译体检的
# FAIL 行归到探针上（注意：FAIL 行只带文件名，所以 compile 列是文件级结论）。
routine_file = {}
routine_files = []
rroot = os.path.join(P, "sql", "routines")
for dp, _, fs in os.walk(rroot):
    for f in sorted(fs):
        if not f.endswith(".sql"):
            continue
        path = os.path.join(dp, f)
        routine_files.append(os.path.relpath(path, P))
        text = open(path, encoding="utf-8", errors="replace").read()
        for m in re.finditer(r"create\s+(?:or\s+replace\s+)?(?:procedure|function)\s+([A-Za-z_][\w#$]*)",
                             text, flags=re.I):
            routine_file.setdefault(m.group(1).lower(), os.path.relpath(path, P))

compile_fail = {}
log = open(compile_log, encoding="utf-8", errors="replace").read()
for line in log.splitlines():
    m = re.match(r"FAIL\s+(\S+)\s+\[([A-Z]+-\d+)\]", line)
    if m:
        compile_fail.setdefault(m.group(1), []).append(m.group(2))

def registrable(f):
    """项目态体检里因解析/编译错误（ENG-*）或编译期方言拒绝（BIZ-0126 中缀
    MOD，2026-11-28 起）失败的文件注册不了，不能进 --file 回退的文件清单；
    仅因体检 gate 2B 的 prepare 失败（KERNEL-*）的文件可以注册。"""
    codes = compile_fail.get(f, [])
    return not codes or not all(
        c.startswith("ENG-") or c == "BIZ-0126" for c in codes
    )

routine_files = [f for f in routine_files if registrable(f)]
miss = set(re.findall(r"MISS \w+:[\w.]+ <- (\w+)", log))

def write_sql(sql):
    qseq[0] += 1
    path = os.path.join(work, f"q{qseq[0]:04d}.sql")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(sql.rstrip().rstrip(";") + ";\n")
    return path

def query(sql, out):
    """跑一条 SQL，结果集落到 out（JSON 快照）。返回 (退出码, 输出文本)。"""
    if os.path.exists(out):
        os.remove(out)
    r = subprocess.run([bin_, "query", "--db", db, "--sql-file", write_sql(sql), "--out", out],
                       capture_output=True, text=True, cwd=P)
    return r.returncode, (r.stdout + r.stderr).strip()

def plan_usable(routine):
    path = os.path.join(P, ".spinduck", "plan", routine + ".toml")
    if not os.path.exists(path):
        return None
    if "[[missing]]" in open(path, encoding="utf-8").read():
        return None
    return path

def call(routine, mode, args, ordered=False):
    """计划可用走 --plan（项目态正路）；否则回退 --file 注册全部例程源文件。
    ordered=True 强制走 --file，且被调方所在文件排在入口文件之前（仓库既有约定：
    标量函数在前、调用方在后），用来对照 X1.plan。"""
    plan = plan_usable(routine)
    nfiles = len(routine_files)
    if plan and not ordered:
        cmd = [bin_, "call", "--plan", plan, "--db", db]
        via = "plan"
    else:
        own = routine_file.get(routine.lower())
        files = [f for f in routine_files if f != own] + [f for f in routine_files if f == own]
        cmd = [bin_, "call", "--db", db]
        for f in files:
            cmd += ["--file", f]
        cmd += [routine]
        nfiles = len(files)
        via = "file(ordered)" if ordered else "file"
    if mode:
        cmd.append("--set-oriented=" + mode)
    cmd += args
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=P)
    shown = cmd[1:] if via == "plan" else ["call", "--db", db, f"--file ×{nfiles}"] + cmd[4:]
    text = "$ " + " ".join(shlex.quote(c) for c in shown) + "\n" + f"[exit {r.returncode}]\n{r.stdout}"
    if r.stderr.strip():
        text += "[stderr]\n" + r.stderr
    # 临时目录的一次性路径不进明细。
    text = text.replace(P, "<project>").replace(work.rstrip("/"), "<work>")
    return r.returncode, r.stdout, via, text

def verdict(stdout, prefix):
    for line in stdout.splitlines():
        if not line.startswith(prefix + " "):
            continue
        body = line[len(prefix) + 1:].split(" ", 1)
        rest = body[1] if len(body) > 1 else ""
        m = re.search(r"reason=(\S+)", rest)
        if "skipped" in rest:
            return "Skipped", (m.group(1) if m else "?")
        if "converted" in rest:
            return "Converted", (rest.replace("converted", "").strip() or "-")
        return rest.split(" ")[0], "-"
    return "(no line)", "-"

def dumps(sql, tag):
    """逐条执行 dump 语句，返回 (全部成功的退出码, [快照文件], [快照文本])。"""
    if not sql.strip():
        return 0, [], []
    paths, texts, rc_all = [], [], 0
    for n, stmt in enumerate(s.strip() for s in sql.split(";;") if s.strip()):
        out = os.path.join(work, f"{tag}_d{n}.json")
        rc, msg = query(stmt, out)
        rc_all |= rc
        if rc == 0 and os.path.exists(out):
            paths.append(out)
            texts.append(open(out, encoding="utf-8").read().strip())
        else:
            paths.append(None)
            texts.append(msg)
    return rc_all, paths, texts

def same(a, b):
    """两份快照用引擎自己的 compare 比对（归一后判值）。任一缺失算不一致。"""
    if a is None or b is None or not os.path.exists(a) or not os.path.exists(b):
        return False
    r = subprocess.run([bin_, "compare", a, b], capture_output=True, text=True)
    return r.returncode == 0

def strip_verdict(text):
    """stdout 比对前去掉 SET_ORIENTED 判定行：集合化模式多这一行，不是结果差异。"""
    return "\n".join(l for l in text.splitlines() if not l.startswith("SET_ORIENTED ")).strip()

def return_value(stdout):
    m = re.search(r"^RETURN (.+)$", stdout, flags=re.M)
    return m.group(1).strip() if m else None

def close(got, want):
    try:
        g, w = float(got), float(want)
    except (TypeError, ValueError):
        return False
    return abs(g - w) <= 1e-6 * max(1.0, abs(w))

rows = []
probes = [l.split("\t") for l in open(manifest, encoding="utf-8").read().splitlines()[1:] if l.strip()]
for raw in probes:
    pid, kind, routine, args, reset, dump, paper, shape = raw[:8]
    oracle = raw[8] if len(raw) > 8 else ""
    expect = raw[9] if len(raw) > 9 else ""
    argl = shlex.split(args)
    row = dict(project=proj, id=pid, kind=kind, routine=routine or "(query)", paper=paper, shape=shape)
    if kind == "sql":
        out = os.path.join(work, f"{pid}_q.json")
        rc, msg = query(dump, out)
        body = open(out, encoding="utf-8").read() if rc == 0 and os.path.exists(out) else msg
        text = f"-- {dump}\n[exit {rc}]\n{body}"
        text = text.replace(P, "<project>").replace(work.rstrip("/"), "<work>")
        open(os.path.join(workdir, f"{pid}.txt"), "w", encoding="utf-8").write(text)
        open(os.path.join(evidence, f"{pid}.txt"), "w", encoding="utf-8").write(text)
        row.update(compile="-", via="query", off="OK" if rc == 0 else f"ERR({rc})",
                   ba="n/a", ba_eq="n/a", st="n/a", st_eq="n/a", off_vs_oracle="n/a", off_vs_expect="n/a",
                   note=re.sub(r"\s+", " ", body.strip())[:110])
        rows.append(row)
        print(f"[probe] [{proj}] {pid:12s} query   -> {row['off']}")
        continue
    f = routine_file.get(routine.lower(), "?")
    row["compile"] = ",".join(sorted(set(compile_fail.get(f, [])))) or "ok"
    stmts = [s.strip() for s in reset.split(";;") if s.strip()]
    ordered = kind == "call-ordered"
    for stmt in stmts:
        query(stmt, os.path.join(work, "reset.json"))
    rc_off, so, via, ev_off = call(routine, None, argl, ordered)
    rc_dump_off, off_paths, off_texts = dumps(dump, pid + "_off")
    off_out = strip_verdict(so)
    row["via"] = via
    snap = [("off", off_paths, off_texts, ev_off)]
    for mode, col in (("batch-atomic", "ba"), ("strict", "st")):
        for stmt in stmts:
            query(stmt, os.path.join(work, "reset.json"))
        rc, so, _, ev = call(routine, mode, argl, ordered)
        st, reason = verdict(so, "SET_ORIENTED")
        _, on_paths, on_texts = dumps(dump, f"{pid}_{col}")
        if rc_off != rc or rc_dump_off != 0 or rc != 0:
            eq = f"EXIT({rc_off}vs{rc})"
        elif dump.strip():
            eq = "EQ" if all(same(a, b) for a, b in zip(off_paths, on_paths)) else "DIFF"
        else:
            eq = "EQ(stdout)" if strip_verdict(so) == off_out else "DIFF(stdout)"
        row[col] = f"{st}/{reason}" if st == "Skipped" else st
        row[col + "_eq"] = eq
        snap.append((mode, on_paths, on_texts, ev))
    row["off"] = "OK" if rc_off == 0 else f"ERR({rc_off})"
    # 参照：Off 的第一份结果快照 与 oracle 查询在同一 DuckDB 上的结果
    row["off_vs_oracle"] = "n/a"
    oracle_text = ""
    if oracle.strip() and rc_off == 0:
        opath = os.path.join(work, f"{pid}_oracle.json")
        rc_o, msg_o = query(oracle, opath)
        oracle_text = open(opath, encoding="utf-8").read().strip() if rc_o == 0 and os.path.exists(opath) else msg_o
        row["off_vs_oracle"] = "EQ" if rc_o == 0 and same(off_paths[0] if off_paths else None, opath) else "DIFF"
    # 期望：函数调用的 RETURN 值与清单里手写的期望；`[ENG|BIZ|KERNEL-####]`
    # 形态的期望 = 编译/运行期显式拒绝（退出码非 0 且输出含该错误码）
    row["off_vs_expect"] = "n/a"
    if expect.strip():
        mcode = re.match(r"^\[(?:ENG|BIZ|KERNEL)-\d+\]", expect.strip())
        if mcode:
            hit = rc_off != 0 and mcode.group(0) in ev_off
            row["off_vs_expect"] = "PASS" if hit else f"FAIL(want {mcode.group(0)})"
        else:
            got = return_value(so) if rc_off == 0 else None
            row["off_vs_expect"] = "PASS" if got is not None and close(got, expect) else f"FAIL({got if got is not None else 'no value'})"
    row["note"] = re.sub(r"\s+", " ", so.strip() if rc_off else off_out)[:110]
    rows.append(row)
    # 每个探针一份合并明细（命令、退出码、stdout/stderr、结果快照、参照）。
    text = ""
    for tag, paths, texts, ev in snap:
        text += f"===== {tag} =====\n{ev.rstrip()}\n"
        for n, stmt in enumerate(x.strip() for x in dump.split(";;") if x.strip()):
            text += f"-- dump[{n}]: {stmt}\n{(texts[n] if n < len(texts) else '(query failed)')}\n"
        text += "\n"
    if oracle.strip():
        text += f"===== oracle =====\n-- {oracle}\n{oracle_text or '(not run: Off failed)'}\n"
    if expect.strip():
        text += f"===== expect =====\n{expect}\n"
    open(os.path.join(workdir, f"{pid}.txt"), "w", encoding="utf-8").write(text)
    # 入库证据：集合化成功、Off 出错、与参照不一致、与期望不符、或 query 通道。
    keep = (row["ba"].startswith("Converted") or rc_off != 0 or row["off_vs_oracle"] == "DIFF"
            or str(row["off_vs_expect"]).startswith("FAIL"))
    if keep:
        open(os.path.join(evidence, f"{pid}.txt"), "w", encoding="utf-8").write(text)
    print(f"[probe] [{proj}] {pid:12s} via={via:13s} off={row['off']:8s} ba={row['ba']:36s} eq={row['ba_eq']:11s} "
          f"oracle={row['off_vs_oracle']:5s} expect={row['off_vs_expect']}")

with open(os.path.join(work, f"rows_{proj}.jsonl"), "w", encoding="utf-8") as fh:
    for r in rows:
        fh.write(json.dumps(r, ensure_ascii=False) + "\n")
with open(os.path.join(work, f"compilefail_{proj}.txt"), "w", encoding="utf-8") as fh:
    for f in sorted(compile_fail):
        fh.write(f"{proj}\t{f}\t{','.join(sorted(set(compile_fail[f])))}\n")
    for name in sorted(miss):
        fh.write(f"{proj}\tMISS(计划闭包缺件)\t{name}\n")
PY
done

# ---- 6b. DuckDB 机制实测：在项目钉住的内核上（经 spinduck query）验证报告里引用的几种写法 ----
MECH_DB=""
for n in $PROJECT_NAMES; do [ -f "$WORK/$n/local.duckdb" ] && MECH_DB="$WORK/$n/local.duckdb"; done
[ -n "$MECH_DB" ] && python3 - "$BIN" "$MECH_DB" "$WORK" "$REPORTS/duckdb_mechanisms.txt" <<'PY'
import os, subprocess, sys
bin_, db, work, out_path = sys.argv[1:5]
mechs = [
 ("TRY(expr)：把运行期转换错误转成 NULL", "SELECT x, TRY(CAST(x AS INTEGER)) AS v FROM (VALUES ('7'), ('x'), (NULL)) t(x)"),
 ("TRY + COALESCE 哨兵：能否区分「出错」与「本来就是 NULL」", "SELECT x, TRY(COALESCE(CAST(x AS INTEGER), '~S~')) IS NULL AS is_err FROM (VALUES ('7'), ('x'), (NULL)) t(x)"),
 ("(DO NOT THROW expr) 写法", "SELECT (DO NOT THROW CAST('x' AS INTEGER)) AS v"),
 ("last_value(v IGNORE NULLS) OVER (…)", "SELECT rn, v, last_value(v IGNORE NULLS) OVER (ORDER BY rn ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS carry FROM (VALUES (1,'a'),(2,NULL),(3,NULL),(4,'b')) t(rn,v)"),
 ("last_value(v) IGNORE NULLS OVER (…) 写法", "SELECT rn, last_value(v) IGNORE NULLS OVER (ORDER BY rn) AS carry FROM (VALUES (1,'a'),(2,NULL),(4,'b')) t(rn,v)"),
 ("lag(v IGNORE NULLS) OVER (…)", "SELECT rn, lag(v IGNORE NULLS) OVER (ORDER BY rn) AS carry FROM (VALUES (1,'a'),(2,NULL),(4,'b')) t(rn,v)"),
 ("窗口 SUM（运行累计）", "SELECT rn, v, SUM(v) OVER (ORDER BY rn ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running FROM (VALUES (1,10),(2,20),(3,5)) t(rn,v)"),
 ("WITH RECURSIVE … USING KEY 配 UNION ALL", "WITH RECURSIVE s USING KEY (k) AS (SELECT k, 0 AS n FROM range(2) t(k) UNION ALL SELECT k, n + 1 FROM s WHERE n < k + 1) SELECT * FROM s ORDER BY k, n"),
 ("WITH RECURSIVE … USING KEY 配 UNION", "WITH RECURSIVE s USING KEY (k) AS (SELECT k, 0 AS n, 10 AS payload FROM range(3) t(k) UNION SELECT k, n + 1, payload - 1 FROM s WHERE n < k + 1) SELECT * FROM s ORDER BY k, n"),
 ("递归 CTE 用布尔标志列终止（Hirn & Grust 编译模板里 rec? 列的写法）", "WITH RECURSIVE run(k, step, go) AS (SELECT 1 AS k, 0 AS step, true AS go UNION ALL SELECT k, step + 1, step + 1 < 3 FROM run WHERE go) SELECT count(*) AS n FROM run"),
]
lines = ["# DuckDB 机制实测：spinduck query（项目钉住的内核）逐条执行；OK 给结果，ERR 给内核首行报错",
         f"# CLI: {subprocess.run([bin_, 'version'], capture_output=True, text=True).stdout.strip()}"]
for i, (label, sql) in enumerate(mechs):
    f = os.path.join(work, f"mech{i:02d}.sql")
    open(f, "w", encoding="utf-8").write(sql + ";\n")
    r = subprocess.run([bin_, "query", "--db", db, "--sql-file", f, "--out", f + ".json"], capture_output=True, text=True)
    if r.returncode == 0 and os.path.exists(f + ".json"):
        rows = open(f + ".json", encoding="utf-8").read().strip().replace("\n", " ")
        lines.append(f"OK   {label}\n       {rows[:300]}")
    else:
        msg = (r.stdout + r.stderr).strip().splitlines()
        lines.append(f"ERR  {label}\n       {msg[0][:200] if msg else '(no output)'}")
open(out_path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
print("\n".join(l[:160] for l in lines[2:]))
PY

# ---- 7. 汇总：合并各工程的行 → 矩阵、汇总数字、编译失败归集 ----
python3 - "$WORK" "$REPORTS" $PROJECT_NAMES <<'PY'
import collections, json, os, sys
work, reports, names = sys.argv[1], sys.argv[2], sys.argv[3:]
rows = []
for n in names:
    p = os.path.join(work, f"rows_{n}.jsonl")
    if os.path.exists(p):
        rows += [json.loads(l) for l in open(p, encoding="utf-8") if l.strip()]
cols = ["project", "id", "paper", "shape", "routine", "compile", "via", "off", "ba", "ba_eq", "st", "st_eq",
        "off_vs_oracle", "off_vs_expect", "note"]
with open(os.path.join(reports, "matrix.tsv"), "w", encoding="utf-8") as fh:
    fh.write("\t".join(cols) + "\n")
    for r in rows:
        fh.write("\t".join(str(r.get(c, "")).replace("\t", " ") for c in cols) + "\n")
with open(os.path.join(reports, "compile_fail.txt"), "w", encoding="utf-8") as fh:
    fh.write("# 项目态编译体检（compile --dir）未通过的源文件 → 错误码；以及计划闭包里的缺件\n")
    for n in names:
        p = os.path.join(work, f"compilefail_{n}.txt")
        if os.path.exists(p):
            fh.write(open(p, encoding="utf-8").read())
out = []
for scope in names + ["all"]:
    rs = rows if scope == "all" else [r for r in rows if r["project"] == scope]
    calls = [r for r in rs if r.get("via") != "query"]
    if not rs:
        continue
    converted = [r["id"] for r in calls if r["ba"] == "Converted"]
    reasons = collections.Counter(r["ba"].replace("Skipped/", "") for r in calls if r["ba"] != "Converted")
    eq = sum(str(r["ba_eq"]).startswith("EQ") for r in calls)
    off_err = [f"{r['id']}:{r['off']}" for r in rs if r["off"] != "OK"]
    orc = collections.Counter(r["off_vs_oracle"] for r in calls if r["off_vs_oracle"] not in ("n/a",))
    exp = collections.Counter(r["off_vs_expect"] for r in calls if r["off_vs_expect"] not in ("n/a",))
    out.append(f"[{scope}] probes={len(rs)} call={len(calls)} query={len(rs) - len(calls)}")
    out.append(f"[{scope}] converted_ba={len(converted)} [{' '.join(converted)}]")
    out.append(f"[{scope}] skipped_ba={len(calls) - len(converted)} " + " ".join(f"{k}={v}" for k, v in sorted(reasons.items(), key=lambda kv: -kv[1])))
    out.append(f"[{scope}] off_vs_ba_eq={eq} of {len(calls)}")
    out.append(f"[{scope}] off_err={len(off_err)} [{' '.join(off_err)}]")
    out.append(f"[{scope}] off_vs_oracle={dict(orc)}")
    out.append(f"[{scope}] off_vs_expect={dict(exp)}")
with open(os.path.join(reports, "summary.txt"), "w", encoding="utf-8") as fh:
    fh.write("\n".join(out) + "\n")
PY

echo "[probe] 矩阵: $REPORTS/matrix.tsv"
echo "[probe] 汇总: $REPORTS/summary.txt"
echo "[probe] 证据: $EVIDENCE/<工程>/（全量明细在 $REPORTS/work/，不入库）"
