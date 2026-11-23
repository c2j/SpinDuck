#!/usr/bin/env python3
"""RBAR 消除的机制证据：同一份已 deploy + 灌数的库副本上，分别以
--set-oriented=off / batch-atomic 跑一次 call，开 --trace-level all，
统计内核侧事件直方图（SQL_EXEC / FETCH / CURSOR_OPEN / IR_STMT）。

计数取自 trace 文本日志的第 4 列（事件名），配套 `<trace>.sql` 侧车给出
去重后的语句条数。这是「为什么快」的证据面，不是墙钟证据面。

用法:
    python3 statement_count.py <purchase|byfund> <N> [mode ...]

前置：同一 N 已跑过 sd_bench.py（需要 WORK/proj/.spinduck/plan/*.toml
与 WORK/db-<dataset>-<N>-off.duckdb）。本脚本把库复制成
db-cnt-<mode>.duckdb 后再跑，不动被测库。

环境变量:
    SPINDUCK_BIN  release CLI（必填）
    WORK          sd_bench.py 的工作目录，默认 /tmp/v3-set-oriented-verify-work
    STEP_BUDGET   call 的 --step-budget，默认 2000000000
"""
import collections
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

BIN = os.environ.get("SPINDUCK_BIN", "")
WORK = Path(os.environ.get("WORK", "/tmp/v3-set-oriented-verify-work"))
STEP_BUDGET = os.environ.get("STEP_BUDGET", "2000000000")
PROC = {
    "purchase": "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
    "byfund": "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
}


def histogram(trace):
    """trace 文本行形如：[trace] TS SEQ EVENT detail。"""
    c = collections.Counter()
    total = 0
    for line in Path(trace).read_text(errors="replace").splitlines():
        parts = line.split()
        if len(parts) >= 4 and parts[0] == "[trace]":
            c[parts[3]] += 1
            total += 1
    return c, total


def run(dataset, n, modes):
    if not BIN:
        sys.exit("需要设置 SPINDUCK_BIN")
    routine = PROC[dataset]
    plan = WORK / "proj" / ".spinduck" / "plan" / f"{routine}.toml"
    src_db = WORK / f"db-{dataset}-{n}-off.duckdb"
    if not plan.exists() or not src_db.exists():
        sys.exit(f"前置缺失：{plan} 或 {src_db}\n请先跑 sd_bench.py {dataset} {n}")
    out = {}
    for mode in modes:
        db = WORK / f"db-cnt-{mode}.duckdb"
        for stale in (str(db), str(db) + ".wal"):
            Path(stale).unlink(missing_ok=True)
        shutil.copy2(src_db, db)
        trace = WORK / f"trace-cnt-{mode}.log"
        for stale in (str(trace), str(trace) + ".sql"):
            Path(stale).unlink(missing_ok=True)
        r = subprocess.run(
            [BIN, "call", "--plan", str(plan), "--db", str(db),
             f"--set-oriented={mode}", "--step-budget", STEP_BUDGET,
             "--trace-level", "all", "--trace", str(trace), "'20250715'", "null"],
            capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"call 失败 mode={mode} rc={r.returncode}\n{r.stdout}\n{r.stderr}")
        c, total = histogram(trace)
        sidecar = Path(str(trace) + ".sql")
        distinct = len([l for l in sidecar.read_text(errors="replace").splitlines()
                        if l.strip()]) if sidecar.exists() else 0
        verdict = [l for l in r.stdout.splitlines() if l.startswith("SET_ORIENTED")]
        out[mode] = dict(events=total, sql_exec=c.get("SQL_EXEC", 0),
                         fetch=c.get("FETCH", 0), cursor_open=c.get("CURSOR_OPEN", 0),
                         ir_stmt=c.get("IR_STMT", 0), distinct_sql=distinct,
                         verdict=verdict)
        print(json.dumps({mode: out[mode]}, ensure_ascii=False), flush=True)
    if "off" in out and len(out) > 1:
        on = {k: v for k, v in out.items() if k != "off"}
        for mode, v in on.items():
            ratio = (out["off"]["sql_exec"] / v["sql_exec"]) if v["sql_exec"] else float("inf")
            print(f"[cnt] {dataset} N={n} SQL_EXEC off={out['off']['sql_exec']} "
                  f"{mode}={v['sql_exec']} 倍数={ratio:.0f}x "
                  f"FETCH off={out['off']['fetch']} {mode}={v['fetch']}")
    (WORK / f"statement-count-{dataset}-{n}.json").write_text(
        json.dumps(dict(dataset=dataset, n=n, modes=out), indent=1, ensure_ascii=False))
    print("CNT_SUMMARY " + json.dumps(dict(dataset=dataset, n=n, modes=out), ensure_ascii=False))


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    run(sys.argv[1], int(sys.argv[2]), sys.argv[3:] or ["off", "batch-atomic"])
