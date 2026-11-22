#!/usr/bin/env bash
# 跑快速 checker，追加一行度量。owner 未指定时只记日志并以 0 退出。
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
LOG=${AMUX_GOAL_LOG:-"$HOME/.amux/logs/spinduck-set-oriented-v3-goal.log"}
PLAN="$ROOT/docs/goals/set-oriented-v3.md"
mkdir -p "$(dirname "$LOG")"
out=$(mktemp "${TMPDIR:-/tmp}/spinduck-v3-goal.XXXXXX")
err=$(mktemp "${TMPDIR:-/tmp}/spinduck-v3-goal.XXXXXX")
"$ROOT/scripts/goals/set-oriented-v3.sh" >"$out" 2>"$err"
code=$?
SPINDUCK_V3_MEASURE_OUT="$out" \
SPINDUCK_V3_MEASURE_ERR="$err" \
SPINDUCK_V3_MEASURE_CODE="$code" \
SPINDUCK_V3_MEASURE_LOG="$LOG" \
SPINDUCK_V3_MEASURE_PLAN="$PLAN" \
python3 - <<'PY'
import json
import os
import sys
from datetime import datetime, timedelta

out = os.environ["SPINDUCK_V3_MEASURE_OUT"]
err = os.environ["SPINDUCK_V3_MEASURE_ERR"]
code = int(os.environ["SPINDUCK_V3_MEASURE_CODE"])
log_path = os.environ["SPINDUCK_V3_MEASURE_LOG"]
plan_path = os.environ["SPINDUCK_V3_MEASURE_PLAN"]

def owner_name():
    try:
        with open(plan_path, encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("owner:"):
                    return line.split(":", 1)[1].strip() or "unassigned"
    except OSError:
        return "unassigned"
    return "unassigned"

def load_checker():
    try:
        with open(out, encoding="utf-8") as handle:
            text = handle.read().strip().splitlines()
    except OSError as exc:
        return None, f"checker output unreadable ({exc.__class__.__name__})"
    if not text:
        err_tail = ""
        try:
            with open(err, encoding="utf-8") as handle:
                err_tail = handle.read().strip().splitlines()[-1][:180]
        except OSError:
            pass
        return None, err_tail or "checker printed nothing"
    try:
        payload = json.loads(text[-1])
    except json.JSONDecodeError:
        return None, "checker stdout is not json"
    if not isinstance(payload, dict) or "done" not in payload or "total" not in payload:
        return None, "checker json has no done/total"
    return payload, None

def prior_rows():
    rows = []
    if not os.path.isfile(log_path):
        return rows
    with open(log_path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return rows

payload, problem = load_checker()
owner = owner_name()
now = datetime.now().astimezone()
stalled = False
regressed = False
if payload is None:
    done, total, summary = None, None, problem
    checker_exit = code if code not in (0, 1) else 2
else:
    done = payload["done"]
    total = payload["total"]
    summary = payload.get("summary", "")
    checker_exit = code
    if code in (0, 1) and isinstance(done, int):
        cutoff = now - timedelta(hours=6)
        baseline = None
        for row in prior_rows():
            ts = row.get("ts")
            if not isinstance(ts, str) or not isinstance(row.get("done"), int):
                continue
            try:
                stamp = datetime.fromisoformat(ts)
            except ValueError:
                continue
            if stamp <= cutoff:
                baseline = row["done"]
        if baseline is not None and done < total:
            stalled = done <= baseline
            regressed = done < baseline

assigned = owner != "unassigned"
if payload is None or checker_exit not in (0, 1):
    measure_exit = 2 if assigned else 0
elif assigned and (stalled or regressed):
    measure_exit = 1
else:
    measure_exit = 0

line = {
    "ts": now.isoformat(timespec="seconds"),
    "done": done,
    "total": total,
    "checker_exit": checker_exit,
    "owner": owner,
    "stalled": stalled,
    "regressed": regressed,
    "summary": summary,
    "measure_exit": measure_exit,
}
encoded = json.dumps(line, ensure_ascii=False)
with open(log_path, "a", encoding="utf-8") as handle:
    handle.write(encoded + "\n")
print(encoded)
sys.exit(measure_exit)
PY
rc=$?
rm -f "$out" "$err"
exit "$rc"
