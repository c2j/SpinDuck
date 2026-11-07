#!/usr/bin/env python3
"""发版一致性守卫：tag 的版本号必须等于 crate 版本（AGENTS §10 产物命名口径）。

为什么需要它：Release 资产名是 `spinduck-<ver>-<os>-<arch>`，`<ver>` 取自 tag；
而二进制内部 `spinduck version` 打印的是 Cargo.toml 版本。两者不一致时，用户看到
的文件名、`--version` 输出与 GitHub Release 标题互相矛盾，且无人报错。本脚本把这条
一致性变成门禁。

用法：
    python3 scripts/ci/check_release_tag.py --print spinduck/Cargo.toml
    python3 scripts/ci/check_release_tag.py --tag v0.1.0 --cargo spinduck/Cargo.toml
    python3 scripts/ci/check_release_tag.py --self-test
"""

from __future__ import annotations

import argparse
import re
import sys

TAG_RE = re.compile(r"^v(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$")
PKG_VERSION_RE = re.compile(r'^version\s*=\s*"([^"]+)"', re.MULTILINE)


def package_version(text: str) -> str:
    """取 `[package]` 段内的第一条 `version`；越过下一个表头即停止搜索。"""
    header = re.search(r"^\[package\]$", text, re.MULTILINE)
    if header is None:
        raise SystemExit("Cargo.toml 里没有 [package] 段")
    rest = text[header.end():]
    nxt = re.search(r"^\[", rest, re.MULTILINE)
    block = rest[: nxt.start()] if nxt else rest
    found = PKG_VERSION_RE.search(block)
    if found is None:
        raise SystemExit("[package] 段没有 version 字段")
    return found.group(1)


def version_from_tag(tag: str) -> str:
    """`v0.1.0` → `0.1.0`；不合规的 tag 直接失败，不猜。"""
    found = TAG_RE.match(tag)
    if found is None:
        raise SystemExit(f"tag {tag!r} 不是 vX.Y.Z 形态（发版 tag 必须以 v 开头带完整三段版本）")
    return found.group(1)


def guard(tag: str, cargo_text: str) -> str:
    from_tag, in_crate = version_from_tag(tag), package_version(cargo_text)
    if from_tag != in_crate:
        raise SystemExit(
            f"tag 版本 {from_tag} != Cargo.toml 版本 {in_crate}："
            "先改 spinduck/Cargo.toml 的 version 并合入，再打 tag（否则 Release 资产名"
            "与二进制内 --version 自相矛盾）"
        )
    # stdout 只给裸版本号（供 workflow 取 output），人读的话留在 stderr
    print(f"版本一致：tag {tag} == crate {in_crate}", file=sys.stderr)
    return in_crate


def self_test() -> int:
    sample = '[package]\nname = "spinduck"\nversion = "0.1.0"\n\n[dependencies]\nduckdb = { version = "=1.10505.0" }\n'
    assert package_version(sample) == "0.1.0", package_version(sample)
    assert version_from_tag("v1.2.3") == "1.2.3"
    assert version_from_tag("v0.1.0-rc.1") == "0.1.0-rc.1"
    assert guard("v0.1.0", sample) == "0.1.0"
    for bad in ("0.1.0", "v0.1", "latest", "polar-mysql-v0.1.0"):
        try:
            version_from_tag(bad)
        except SystemExit:
            pass
        else:
            raise AssertionError(f"tag {bad!r} 本应被拒")
    try:
        guard("v9.9.9", sample)
    except SystemExit:
        pass
    else:
        raise AssertionError("版本不一致本应失败")
    print("self-test ok")
    return 0


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="check_release_tag.py")
    ap.add_argument("--tag")
    ap.add_argument("--cargo", default="spinduck/Cargo.toml")
    ap.add_argument("--print", dest="print_only", metavar="CARGO_TOML")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv[1:])
    if args.self_test:
        return self_test()
    if args.print_only:
        print(package_version(open(args.print_only, encoding="utf-8").read()))
        return 0
    if not args.tag:
        ap.error("需要 --tag，或用 --print/--self-test")
    print(guard(args.tag, open(args.cargo, encoding="utf-8").read()))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
