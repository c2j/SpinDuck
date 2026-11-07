#!/usr/bin/env python3
"""发版产物门禁：断言 ELF 二进制要求的 GLIBC 不高于策略上限（AGENTS §10）。

为什么需要它：`cargo zigbuild --target ....2.28` 只是"请求"锚定符号版本，构建机
glibc 更高时链接器不会拦你——约束必须由产物的实际版本化符号反查，否则 RHEL8
（glibc 2.28）上跑不起来却已经发布出去了。本脚本读 `readelf -V`（退回 `objdump -T`）
取 `GLIBC_x.y[.z]` 的最大值，超限即非零退出。

用法：
    python3 scripts/ci/check_max_glibc.py <elf-path> [--max 2.28.0]
    python3 scripts/ci/check_max_glibc.py --self-test
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys

DEFAULT_MAX = (2, 28, 0)
GLIBC_RE = re.compile(r"GLIBC_(\d+)\.(\d+)(?:\.(\d+))?")


def parse_version(text: str) -> tuple[int, int, int]:
    """`"2.28"` / `"2.28.0"` → 三元组；缺位补 0。"""
    parts = [int(p) for p in text.split(".")]
    while len(parts) < 3:
        parts.append(0)
    if len(parts) != 3:
        raise ValueError(f"非法版本：{text!r}")
    return (parts[0], parts[1], parts[2])


def glibc_versions(text: str) -> list[tuple[int, int, int]]:
    """从 readelf/objdump 输出文本里抽出全部 GLIBC 符号版本。"""
    return [(int(m[0]), int(m[1]), int(m[2] or 0)) for m in GLIBC_RE.findall(text)]


def max_version(versions: list[tuple[int, int, int]]):
    return max(versions) if versions else None


def label(version: tuple[int, int, int]) -> str:
    major, minor, patch = version
    return f"GLIBC_{major}.{minor}.{patch}" if patch else f"GLIBC_{major}.{minor}"


def symbol_text(path: str) -> str:
    """优先 readelf，缺失或无输出时退回 objdump；两者皆不可用则报错退出。"""
    errors = []
    for cmd in (["readelf", "-V", path], ["objdump", "-T", path]):
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True)
        except OSError as exc:
            errors.append(f"{cmd[0]}: {exc}")
            continue
        if proc.returncode == 0 and proc.stdout.strip():
            return proc.stdout
        errors.append(f"{cmd[0]} 退出 {proc.returncode}: {(proc.stderr or '').strip()}")
    raise SystemExit(f"需要 readelf 或 objdump 才能检查 {path}：" + "；".join(errors))


def check(path: str, maximum: tuple[int, int, int]) -> int:
    versions = glibc_versions(symbol_text(path))
    highest = max_version(versions)
    if highest is None:
        raise SystemExit(f"{path} 里没有 GLIBC 版本化符号（不是预期的 Linux ELF？）")
    print(f"{path}: 需要 {label(highest)}（策略上限 {label(maximum)}）")
    if highest > maximum:
        raise SystemExit(f"{path} 要求 {label(highest)}，超出 {label(maximum)}")
    return 0


def self_test() -> int:
    ok = glibc_versions("GLIBC_2.2.5 x\nGLIBC_2.27 y\nGLIBC_2.28 fcntl64\n")
    assert max_version(ok) == (2, 28, 0), ok
    assert (2, 28, 0) <= DEFAULT_MAX and (2, 31, 0) > DEFAULT_MAX
    assert max_version(glibc_versions("无版本化符号")) is None
    assert parse_version("2.28") == (2, 28, 0) and parse_version("2.39.6") == (2, 39, 6)
    assert label((2, 2, 5)) == "GLIBC_2.2.5" and label((2, 28, 0)) == "GLIBC_2.28"
    print("self-test ok")
    return 0


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="check_max_glibc.py")
    ap.add_argument("elf", nargs="?")
    ap.add_argument("--max", default="2.28.0", help="允许的 GLIBC 上限（默认 2.28.0）")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv[1:])
    if args.self_test:
        return self_test()
    if not args.elf:
        ap.error("需要 <elf-path>，或用 --self-test")
    return check(args.elf, parse_version(args.max))


if __name__ == "__main__":
    sys.exit(main(sys.argv))
