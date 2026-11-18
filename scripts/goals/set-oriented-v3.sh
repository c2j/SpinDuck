#!/usr/bin/env bash
# 集合化 V3 快速验收。0 全满足，1 有缺口，2 有一行无法判定。
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
export SPINDUCK_V3_ROOT="$ROOT"
exec python3 - "$@" <<'PY'
import json
import os
import re
import subprocess
import sys

ROOT = os.environ["SPINDUCK_V3_ROOT"]
TOTAL = 6
BUDGET_S = 90
M1 = "should_enforce_write_buffer_immediate_policy_for_qfii_cross_table_and_fxj_close"
M2 = [
    "should_drive_multiple_regions_with_run_span_and_preserve_suffix_absolute_gotos",
    "should_reject_outer_and_inner_when_static_cursors_are_nested",
    "should_keep_prc_trd_deal_purchase_single_region_conversion_unchanged",
    "should_inline_multi_out_lookup_tuple_and_reject_unregistered_or_partial_out",
    "should_lower_jyfy_aggregate_with_exact_four_case_semantics",
    "should_fallback_when_v_par_bond_has_zero_overlapping_or_query_error",
    "should_reject_sys_dummy_block_when_not_total_and_verify_decimal_mul_sub",
    "should_require_zero_bad_rows_when_inst_log_is_non_noop",
    "should_convert_region0_of_byfund_and_match_off_on_small_corpus",
]
CI_JOBS = ("format", "clippy", "test", "audit", "golden")
GOLDEN_PATHS = [
    "spinduck/src/ir/set_oriented",
    "spinduck/src/engine/set_oriented.rs",
    "spinduck/src/ir/types.rs",
    "spinduck/src/runtime/interp/writebuf.rs",
    "spinduck/src/runtime/interp.rs",
]
M0_KEYS = ("prefix_ms", "c_get_data_ms", "c_fxj_ms", "qfii_ms")
rows = []


def git(*args, timeout=15):
    try:
        return subprocess.run(
            ["git", *args],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return exc


def record(row_id, state, detail):
    rows.append({"id": row_id, "state": state, "detail": detail})


def head_sha():
    result = git("rev-parse", "HEAD")
    if not isinstance(result, subprocess.CompletedProcess) or result.returncode != 0:
        return None
    return result.stdout.strip()


def defined(name):
    needle = f"fn {name}"
    result = subprocess.run(
        ["grep", "-R", "-F", "-q", "--include=*.rs", needle, "spinduck"],
        cwd=ROOT,
    )
    return result.returncode == 0


def main():
    import time

    deadline = time.monotonic() + BUDGET_S
    sha = head_sha()
    design = git("cat-file", "-e", "HEAD:docs/spinduck-set-oriented-v3-design.md")
    if sha is None or not isinstance(design, subprocess.CompletedProcess):
        record(1, "unmeasured", "git rev-parse or cat-file failed")
    elif design.returncode == 0:
        record(1, "met", sha)
    else:
        record(1, "gap", "design is not in HEAD")

    check_m0(sha)
    check_tests(3, [M1], deadline)
    check_tests(4, M2, deadline)
    check_golden(sha)
    check_ci(sha)
    emit()


def check_m0(sha):
    path = os.path.join(ROOT, "docs/goals/artifacts/set-oriented-v3-m0-profile.json")
    data, problem = load_json(path)
    if problem == "missing":
        record(2, "gap", "m0 artifact missing")
        return
    if problem:
        record(2, "gap", f"m0 artifact {problem}")
        return
    problem = validate_m0(data)
    if problem:
        record(2, "gap", problem)
        return
    if sha is None:
        record(2, "unmeasured", "cannot compare m0 git_head without HEAD")
        return
    state, detail = ancestor_state(data["git_head"], sha, "m0 git_head")
    record(2, state, detail)


def validate_m0(data):
    if data.get("routine") != "prc_trd_hz_byfund":
        return "m0 routine is not prc_trd_hz_byfund"
    if not isinstance(data.get("git_head"), str) or not re.fullmatch(r"[0-9a-f]{40}", data["git_head"]):
        return "m0 git_head is not a 40-char sha"
    builds = data.get("builds")
    if not isinstance(builds, dict):
        return "m0 builds missing"
    for mode in ("debug", "release"):
        block = builds.get(mode)
        if not isinstance(block, dict):
            return f"m0 builds.{mode} missing"
        for key in M0_KEYS:
            value = block.get(key)
            if not isinstance(value, (int, float)) or isinstance(value, bool) or value < 0:
                return f"m0 builds.{mode}.{key} is not a number >= 0"
    return None


def check_golden(sha):
    path = os.path.join(ROOT, "docs/goals/artifacts/set-oriented-v3-golden.json")
    data, problem = load_json(path)
    if problem == "missing":
        record(5, "gap", "golden artifact missing")
        return
    if problem:
        record(5, "gap", f"golden artifact {problem}")
        return
    problem = validate_golden(data)
    if problem:
        record(5, "gap", problem)
        return
    if sha is None:
        record(5, "unmeasured", "cannot compare golden git_head without HEAD")
        return
    artifact_head = data["git_head"]
    if artifact_head == sha:
        record(5, "met", artifact_head)
        return
    state, detail = ancestor_state(artifact_head, sha, "golden git_head")
    if state != "met":
        record(5, state, detail)
        return
    diff = git("diff", "--name-only", artifact_head, sha, "--", *GOLDEN_PATHS)
    if not isinstance(diff, subprocess.CompletedProcess) or diff.returncode != 0:
        record(5, "unmeasured", "git diff of golden paths failed")
        return
    changed = [line for line in diff.stdout.splitlines() if line]
    if changed:
        record(5, "gap", "golden stale after " + ", ".join(changed))
        return
    record(5, "met", artifact_head)


def validate_golden(data):
    if not isinstance(data.get("git_head"), str) or not re.fullmatch(r"[0-9a-f]{40}", data["git_head"]):
        return "golden git_head is not a 40-char sha"
    if data.get("input_rows") != 100000:
        return "golden input_rows is not 100000"
    if data.get("p_o_succeed") != 0:
        return "golden p_o_succeed is not 0"
    if data.get("off_matches_gauss") is not True or data.get("batch_atomic_matches_gauss") is not True:
        return "golden match flags are not both true"
    return None


def ancestor_state(older, newer, label):
    if not re.fullmatch(r"[0-9a-f]{40}", older):
        return "gap", f"{label} is not a sha"
    seen = git("cat-file", "-e", older)
    if not isinstance(seen, subprocess.CompletedProcess):
        return "unmeasured", f"git cat-file {label} failed"
    if seen.returncode != 0:
        return "gap", f"{label} is not in this repo"
    anc = git("merge-base", "--is-ancestor", older, newer)
    if not isinstance(anc, subprocess.CompletedProcess):
        return "unmeasured", f"git merge-base {label} failed"
    if anc.returncode == 0:
        return "met", older
    if anc.returncode == 1:
        return "gap", f"{label} is not an ancestor of HEAD"
    return "unmeasured", f"git merge-base {label} exit {anc.returncode}"


def load_json(path):
    if not os.path.isfile(path):
        return None, "missing"
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle), None
    except (OSError, json.JSONDecodeError) as exc:
        return None, f"unreadable ({exc.__class__.__name__})"


def check_tests(row_id, names, deadline):
    import time

    missing = [name for name in names if not defined(name)]
    if missing:
        shown = ", ".join(missing[:3])
        extra = f" (+{len(missing) - 3})" if len(missing) > 3 else ""
        record(row_id, "gap", f"{len(missing)} test(s) missing: {shown}{extra}")
        return
    for name in names:
        left = deadline - time.monotonic()
        if left < 5:
            record(row_id, "unmeasured", f"cargo budget exhausted before {name}")
            return
        state, detail = cargo_exact(name, left)
        if state != "met":
            record(row_id, state, detail)
            return
    record(row_id, "met", f"{len(names)} passed")


def cargo_exact(name, timeout):
    try:
        result = subprocess.run(
            ["cargo", "test", "--features", "cli", name, "--", "--exact"],
            cwd=os.path.join(ROOT, "spinduck"),
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except FileNotFoundError:
        return "unmeasured", "cargo not installed"
    except subprocess.TimeoutExpired:
        return "unmeasured", f"cargo timed out on {name}"
    text = result.stdout + "\n" + result.stderr
    passed = f"test {name} ... ok" in text or re.search(
        rf"test \S*{re.escape(name)} \.\.\. ok", text
    )
    if result.returncode == 0 and passed:
        return "met", name
    if "could not compile" in text or result.returncode == 101 and "error[" in text and "FAILED" not in text:
        return "unmeasured", f"cargo could not compile before {name}"
    tail = next((line for line in reversed(text.splitlines()) if line.strip()), "cargo failed")
    return "gap", f"{name} failed: {tail[:180]}"


def check_ci(sha):
    if sha is None:
        record(6, "unmeasured", "cannot compare PR head without HEAD")
        return
    repo = origin_repo()
    if repo is None:
        record(6, "unmeasured", "origin is not a GitHub repo URL")
        return
    payload, problem = gh_json(
        ["pr", "list", "--repo", repo, "--head", "feat/set-oriented-v3", "--state", "open",
         "--json", "number,headRefOid,statusCheckRollup,url"]
    )
    if problem:
        record(6, "unmeasured", problem)
        return
    if not isinstance(payload, list):
        record(6, "unmeasured", "gh pr list did not return a list")
        return
    if len(payload) == 0:
        record(6, "gap", "no open PR")
        return
    if len(payload) > 1:
        record(6, "unmeasured", f"{len(payload)} open PRs for this branch")
        return
    pr = payload[0]
    if pr.get("headRefOid") != sha:
        record(6, "gap", f"PR #{pr.get('number')} is not at HEAD")
        return
    rollup = pr.get("statusCheckRollup") or []
    found = {}
    for check in rollup:
        if not isinstance(check, dict):
            continue
        name = check.get("name")
        workflow = check.get("workflowName")
        if name not in CI_JOBS:
            continue
        if workflow not in (None, "", "ci"):
            continue
        found[name] = check
    missing = [name for name in CI_JOBS if name not in found]
    bad = []
    for name in CI_JOBS:
        check = found.get(name)
        if not check:
            continue
        if check.get("status") == "COMPLETED" and check.get("conclusion") == "SUCCESS":
            continue
        bad.append(f"{name}={check.get('status')}/{check.get('conclusion')}")
    if missing or bad:
        detail = []
        if missing:
            detail.append("missing " + ",".join(missing))
        if bad:
            detail.append("not green " + ",".join(bad))
        record(6, "gap", "CI " + "; ".join(detail))
        return
    record(6, "met", pr.get("url") or f"#{pr.get('number')}")


def origin_repo():
    result = git("remote", "get-url", "origin")
    if not isinstance(result, subprocess.CompletedProcess) or result.returncode != 0:
        return None
    match = re.search(r"github\.com[:/]([^/]+)/([^/.\s]+)", result.stdout.strip())
    if not match:
        return None
    return f"{match.group(1)}/{match.group(2)}"


def gh_json(args):
    try:
        result = subprocess.run(
            ["gh", *args],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=20,
        )
    except FileNotFoundError:
        return None, "gh not installed"
    except subprocess.TimeoutExpired:
        return None, "gh timed out"
    if result.returncode != 0:
        err = (result.stderr or result.stdout or "").strip().splitlines()
        return None, (err[-1] if err else f"gh exit {result.returncode}")[:180]
    try:
        return json.loads(result.stdout), None
    except json.JSONDecodeError:
        return None, "gh returned non-json"


def emit():
    done = [row for row in rows if row["state"] == "met"]
    gaps = [row for row in rows if row["state"] == "gap"]
    unknown = [row for row in rows if row["state"] == "unmeasured"]
    parts = []
    if unknown:
        listed = ", ".join(f"row {row['id']} ({row['detail']})" for row in unknown)
        parts.append("unmeasured: " + listed)
    if gaps:
        ids = ", ".join(str(row["id"]) for row in gaps)
        parts.append(f"{len(gaps)} real gaps: rows {ids}")
    summary = "; ".join(parts) if parts else "all rows met"
    print(json.dumps({
        "done": len(done),
        "total": TOTAL,
        "summary": summary,
        "rows": rows,
    }, ensure_ascii=False))
    if unknown:
        sys.exit(2)
    if gaps:
        sys.exit(1)
    sys.exit(0)


if __name__ == "__main__":
    main()
PY
