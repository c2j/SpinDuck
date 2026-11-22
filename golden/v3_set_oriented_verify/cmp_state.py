#!/usr/bin/env python3
"""跨引擎产物一致性核对：同一张表在 DuckDB(SpinDuck) 与 openGauss(参照) 上跑
同一组聚合（行数 + 每个数值列 SUM/MIN/MAX + 每个字符列 COUNT DISTINCT/MIN/MAX），
逐值比对。

为什么用聚合而不是逐行 CSV 比对：两侧的类型呈现（数字定标、日期格式、
NULL 拼写）本就不一致，CSV 差异里绝大部分是序列化漂移；聚合把「业务面等价」
压成一串可逐值核对的标量，同时覆盖全表内容。行级等价由 sd_bench.py 的
OFF/ON 同库摘要（sha256）负责，两者互补。

用法:
    python3 cmp_state.py <db.duckdb> <ref_db> <table> [table ...]

环境变量:
    SPINDUCK_BIN  release CLI（必填）
    CONTAINER     openGauss 容器名，默认 sd-og70
    GS_USER       参照侧用户，默认 gaussdb（口令只在容器内取 $GS_PASSWORD）

注意：参照侧库必须已部署同一份语料对象并灌过同一份种子（reference/sp_deploy.sh）。
"""
from pathlib import Path
import json
import os
import subprocess
import sys

BIN = os.environ.get("SPINDUCK_BIN", "")
CONTAINER = os.environ.get("CONTAINER", "sd-og70")
GS_USER = os.environ.get("GS_USER", "gaussdb")
WORK = Path(os.environ.get("CMP_WORK", "/tmp/v3-set-oriented-verify-cmp"))

NUMERIC_HINTS = ("numeric", "int", "decimal", "float", "double", "number")


def sd_query(db, sql, tag):
    WORK.mkdir(parents=True, exist_ok=True)
    f = WORK / (tag + ".sql")
    out = WORK / (tag + ".json")
    f.write_text(sql)
    r = subprocess.run([BIN, "query", "--db", str(db), "--sql-file", str(f), "--out", str(out)],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"spinduck query 失败 {tag}: {r.stdout}{r.stderr}")
    return json.loads(out.read_text())["rows"]


def ref_query(db, sql):
    """参照侧执行：口令从容器环境变量取，不进命令行、不落文件。"""
    inner = ('export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$LD_LIBRARY_PATH; '
             f'/usr/local/opengauss/bin/gsql -U {GS_USER} -W "$GS_PASSWORD" '
             f'-d {db} -p 5432 -tA -v ON_ERROR_STOP=1')
    r = subprocess.run(["docker", "exec", "-i", CONTAINER, "bash", "-lc", inner],
                       capture_output=True, text=True, input=sql)
    if r.returncode != 0:
        raise SystemExit(f"ref query 失败: {r.stdout}{r.stderr}\nSQL: {sql[:300]}")
    return r.stdout.strip().split("|")


def columns(db, table):
    return sd_query(db, "SELECT column_name, data_type FROM information_schema.columns"
                        f" WHERE table_name='{table}' ORDER BY ordinal_position", f"cols-{table}")


def agg_batches(table, cols, batch=40):
    parts = ["count(*)"]
    for name, typ in cols:
        if any(k in str(typ).lower() for k in NUMERIC_HINTS):
            parts += [f'sum("{name}"::numeric)', f'min("{name}"::numeric)', f'max("{name}"::numeric)']
        else:
            parts += [f'count(distinct "{name}")', f'min("{name}")', f'max("{name}")']
    return [f'SELECT {", ".join(parts[i:i + batch])} FROM {table};'
            for i in range(0, len(parts), batch)]


def norm(vals):
    out = []
    for v in vals:
        if v is None or v == "" or str(v).lower() == "null":
            out.append(None)
            continue
        try:
            out.append(round(float(v), 3))
        except ValueError:
            out.append(str(v).strip())
    return out


def main():
    if len(sys.argv) < 4 or not BIN:
        sys.exit(__doc__)
    db, refdb, tables = sys.argv[1], sys.argv[2], sys.argv[3:]
    ok = True
    for t in tables:
        for i, sql in enumerate(agg_batches(t, columns(db, t))):
            a = norm(sd_query(db, sql, f"sd-{t}-{i}")[0])
            b = norm(ref_query(refdb, sql))
            same = a == b
            ok = ok and same
            print(f'{"EQ  " if same else "DIFF"} {t} batch{i} sd={a[:6]} ref={b[:6]}')
            if not same:
                print("   首个差异位:", [j for j in range(min(len(a), len(b))) if a[j] != b[j]][:12])
    print("STATE_EQUAL" if ok else "STATE_DIFF", tables)
    sys.exit(0 if ok else 1)


main()
