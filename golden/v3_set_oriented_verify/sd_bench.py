#!/usr/bin/env python3
"""PR #59 集合化 V3 三方基准 —— SpinDuck 侧驱动（存过 vs 非集合化 vs 集合化）。

用法: python3 sd_bench.py <purchase|byfund> <N> [mode ...]
    mode ∈ {off, strict, batch-atomic}，默认 off + batch-atomic。

环境变量:
    SPINDUCK_BIN  release CLI 路径（必填，通常由 run_verify.sh 注入）
    CORPUS        被测语料工程根（PR #59 head 下的 golden/case_corpus_purchase）
    WORK          工作目录（duckdb 文件 + 每步日志），默认 /tmp/v3-set-oriented-verify-work
    STEP_BUDGET   call 的 --step-budget，默认 2000000000

每个 mode 独立 deploy + 种子 + 生成 N 行驱动数据 → 只计 call 墙钟 →
导出全库状态（逐表行集排序后的 sha256），供 OFF/ON 比对。输出为 JSON 行 +
末行 BENCH_SUMMARY，另写 WORK/log-<dataset>-<N>/summary.json。
"""
from pathlib import Path
import hashlib
import json
import os
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
BIN = os.environ.get("SPINDUCK_BIN", "")
CORPUS = Path(os.environ.get("CORPUS", ""))
WORK = Path(os.environ.get("WORK", "/tmp/v3-set-oriented-verify-work"))
STEP_BUDGET = os.environ.get("STEP_BUDGET", "2000000000")
BUSINESS_TABLES = ("dat_trd_repurchase", "dat_fund_cjqs", "dat_fund_xw_venture")
PROC = {
    "purchase": "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
    "byfund": "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
}

if not BIN or not CORPUS:
    sys.exit("需要设置 SPINDUCK_BIN 与 CORPUS（建议经 run_verify.sh 调用）")


def run(args, label, logdir):
    """跑一次 CLI，记墙钟；非零退出即终止（不复用错误状态继续测时）。"""
    t0 = time.perf_counter()
    r = subprocess.run([BIN] + args, capture_output=True, text=True)
    (logdir / f"{label}.log").write_text(r.stdout + r.stderr)
    if r.returncode != 0:
        sys.exit(f"{label} exit={r.returncode}\n{r.stdout}\n{r.stderr}")
    return r.stdout, (time.perf_counter() - t0) * 1000


def query(db, sql, label, logdir):
    (logdir / f"{label}.sql").write_text(sql)
    out = logdir / f"{label}.json"
    run(["query", "--db", str(db), "--sql-file", str(logdir / f"{label}.sql"),
         "--out", str(out)], label, logdir)
    return json.loads(out.read_text())


def sql_file(name, n):
    """读 sql/ 下的模板，把 :N 占位替换为实际行数后按语句拆分。"""
    text = (HERE / "sql" / name).read_text().replace(":N", str(n))
    return [s.strip() + ";" for s in text.split(";\n") if s.strip()]


def materialize_project(dataset):
    """把语料工程复制到 WORK（排除缓存），并确保 byfund 入口已登记。

    PR #59 的 spinduck.toml 只登记 purchase 入口，本包要核验 byfund 时
    在副本上追加——不改被测 worktree；若上游已登记则原样沿用。
    """
    proj = WORK / "proj"
    proj.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["rsync", "-a", "--delete", "--exclude", ".spinduck",
                    "--exclude", ".codeweb", "--exclude", "reports",
                    f"{CORPUS}/", f"{proj}/"], check=True)
    toml_path = proj / "spinduck.toml"
    toml = toml_path.read_text()
    if dataset == "byfund" and "prc_trd_hz_byfund" not in toml:
        toml += ('\n# 本包追加：byfund 与 purchase 同为待核验入口。\n'
                 '[[entry]]\nroutine = "pkg_split_trade_step3_sh.prc_trd_hz_byfund"\n'
                 'kind = "procedure"\n')
        toml_path.write_text(toml)
    return proj


def compile_plan(proj, routine, logdir):
    """只编译本次要跑的过程：主仓语料在 PR #59 合并前缺 byfund 依赖桩，
    顺带编译另一条会整轮失败，按 dataset 精确编译才允许单过程复现。"""
    out, _ = run(["compile", "--dir", str(proj), "--analyze", "--entry", routine],
                 f"compile-{routine.split('.')[-1]}", logdir)
    plan_path = next((Path(line.split()[1]) for line in out.splitlines()
                      if line.startswith("plan: ")), None)
    if plan_path is None:
        sys.exit(f"compile 未产出 plan：{routine}\n{out}")
    missing = [line for line in out.splitlines() if "missing=" in line]
    print(f"[compile] {routine} {missing[-1] if missing else ''}", flush=True)
    return plan_path


def seed_params(db, dataset, logdir):
    """业务参数种子：只取非 mid_yjqs_detail 的 INSERT（驱动数据另有生成器）。"""
    seed = (CORPUS / "cases" / ("datasets_byfund" if dataset == "byfund" else "datasets")
            / "seed.sql").read_text()
    stmts = [s.strip() + ";" for s in seed.split(";")
             if "INSERT" in s.upper() and "mid_yjqs_detail" not in s and s.strip()]
    if dataset == "byfund":
        stmts += [s.strip() + ";" for s in (HERE / "sql" / "byfund_params_extra.sql").read_text().split(";")
                  if s.strip()]
    for i, s in enumerate(stmts):
        query(db, s, f"param-{i}", logdir)


def cursor_groups(db, dataset, logdir):
    """计时前核对主游标实际产出的循环规模：purchase 逐行 / byfund 分组数。"""
    if dataset == "purchase":
        sql = ("SELECT count(*) FROM mid_yjqs_detail WHERE bcrq='20250715' AND scdm='001'"
               " AND stock_kind NOT LIKE '0509%' AND cjsl > 0")
    else:
        # 不带 fnc_get_fund_data_mode 谓词：query 子命令不注册语料函数；
        # 该谓词按构造恒真，循环规模由产物表 dat_fund_cjqs 行数佐证。
        sql = ("SELECT count(*) FROM (SELECT security_id FROM mid_yjqs_detail"
               " WHERE bcrq='20250715' AND scdm='001'"
               " AND NOT (is_yysg='1' AND ywlb IN ('ES','77'))"
               " GROUP BY fund_code,xwdm,bs,security_id,stock_kind,scdm,trade_type,"
               " pay_type,main_xwdm,gddm,sub_gddm,check_type,date_source,cjbz,zqdm)")
    return int(query(db, sql, "groups", logdir)["rows"][0][0])


def full_state(db, label, logdir):
    """全库状态摘要：每张业务表行集排序后并入 dict，整体 sha256。"""
    tables = [r[0] for r in query(
        db, "SELECT table_name FROM information_schema.tables"
            " WHERE table_schema='main' AND table_type='BASE TABLE' ORDER BY 1",
        f"{label}-tables", logdir)["rows"] if not r[0].startswith("sd_")]
    state = {}
    for t in tables:
        rows = query(db, f'SELECT * FROM "{t}"', f"{label}-t-{t}", logdir)["rows"]
        rows.sort(key=lambda x: json.dumps(x, ensure_ascii=False))
        state[t] = rows
    digest = hashlib.sha256(
        json.dumps(state, sort_keys=True, ensure_ascii=False).encode()).hexdigest()
    return tables, state, digest


def run_mode(dataset, n, mode, plan, logdir):
    """每个 mode 都独立 deploy + 同一份种子 + 同一份生成数据，保证可比。"""
    db = WORK / f"db-{dataset}-{n}-{mode}.duckdb"
    for stale in (str(db), str(db) + ".wal"):
        Path(stale).unlink(missing_ok=True)
    run(["deploy", str(plan), "--db", str(db)], f"{mode}-deploy", logdir)
    seed_params(db, dataset, logdir)
    for i, stmt in enumerate(sql_file(f"gen_{dataset}.sql", n)):
        query(db, stmt, f"gen-{i}", logdir)
    groups = cursor_groups(db, dataset, logdir)
    out, ms = run(["call", "--plan", str(plan), "--db", str(db),
                   f"--set-oriented={mode}", "--step-budget", STEP_BUDGET,
                   "'20250715'", "null"], f"{mode}-call", logdir)
    tables, state, digest = full_state(db, mode, logdir)
    return dict(elapsed_ms=round(ms, 1), groups=groups, mode=mode,
                verdict=[l for l in out.splitlines() if l.startswith("SET_ORIENTED")],
                outs=[l for l in out.splitlines() if l.startswith("OUT")],
                tables=len(tables), sha=digest,
                rows={k: len(v) for k, v in state.items() if k in BUSINESS_TABLES})


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    dataset, n = sys.argv[1], int(sys.argv[2])
    modes = sys.argv[3:] or ["off", "batch-atomic"]
    routine = PROC[dataset]
    WORK.mkdir(parents=True, exist_ok=True)
    logdir = WORK / f"log-{dataset}-{n}"
    logdir.mkdir(parents=True, exist_ok=True)

    plan = compile_plan(materialize_project(dataset), routine, logdir)
    results = {}
    for mode in modes:
        results[mode] = run_mode(dataset, n, mode, plan, logdir)
        print(json.dumps({dataset: {mode: results[mode]}}, ensure_ascii=False), flush=True)

    shas = {r["sha"] for r in results.values()}
    summary = dict(dataset=dataset, n=n, routine=routine, modes=results,
                   state_equal=len(results) > 1 and len(shas) == 1)
    (logdir / "summary.json").write_text(json.dumps(summary, indent=1, ensure_ascii=False))
    print("BENCH_SUMMARY " + json.dumps(summary, ensure_ascii=False), flush=True)


main()
