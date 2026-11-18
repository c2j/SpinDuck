#!/usr/bin/env python3
"""M0 剖面：从 PKG_SPLIT_TRADE_STEP3_SH.sql 逐字抽出 prc_trd_hz_byfund 过程体，
生成 3 个"截断变体" + 1 个空过程，供分段计时（累计墙钟差分）。

切点（与设计 §5.2 四段口径对齐，按语句文本定位而非行号）：
  prc_m0_prefix    : 到 `OPEN c_get_data;` 之前        → 前缀（DELETE×2 + UPDATE×2）
  prc_m0_cgetdata  : 到 `OPEN c_get_venture_data;` 之前 → + c_get_data 游标循环
  prc_m0_cfxj      : 到 QFII `IF p_i_date >= fnc_get_split_date` 之前
                     → + c_get_venture_data/c_fxj 动态游标/c_get_unknown
  （全程 = 原包内 pkg_split_trade_step3_sh.prc_trd_hz_byfund，不另生成）
  prc_m0_noop      : 空过程，估 CLI 调用固定开销（进程+开库+注册+事务）

用法：golden/set_oriented_v3_m0/gen_variants.py <PKG_SPLIT_TRADE_STEP3_SH.sql> > variants.sql
"""
import sys

PKG = sys.argv[1]
text = open(PKG, encoding="utf-8").read()

# 过程体在 PACKAGE BODY 内（spec 里同名声明不带 IS 块），从 BODY 之后定位
body_at = text.index("PACKAGE BODY")
START = text.index("PROCEDURE prc_trd_hz_byfund", body_at)
# 过程体终于下一个 `PROCEDURE prc_trd_deal_purchase` 之前
END = text.index("PROCEDURE prc_trd_deal_purchase", START)
block = text[START:END].rstrip() + "\n"
lines = block.splitlines(keepends=True)


def cut_index(marker: str) -> int:
    hits = [i for i, ln in enumerate(lines) if marker in ln]
    if len(hits) != 1:
        sys.exit(f"marker {marker!r} 命中 {len(hits)} 处，需唯一")
    return hits[0]


CUT_PREFIX = cut_index("OPEN c_get_data;")
CUT_CGETDATA = cut_index("OPEN c_get_venture_data;")
CUT_CFXJ = cut_index("IF p_i_date >= fnc_get_split_date")
assert CUT_PREFIX < CUT_CGETDATA < CUT_CFXJ


def variant(name: str, cut: int) -> str:
    head = lines[0].replace("PROCEDURE prc_trd_hz_byfund", f"CREATE OR REPLACE PROCEDURE {name}", 1)
    body = "".join(lines[1:cut])
    # 截断点均为主 BEGIN 块深度 1 的语句边界：补 RETURN 后闭合 END ;
    return head + body + "    RETURN;\n  END ;\n"


out = [
    "-- M0 剖面分段计时变体（由 golden/set_oriented_v3_m0/gen_variants.py 生成，勿手改）\n",
    "CREATE OR REPLACE PROCEDURE prc_m0_noop(p_i_date IN VARCHAR2, p_o_succeed OUT VARCHAR2) IS\n",
    "BEGIN\n    p_o_succeed := '0';\n    RETURN;\n  END ;\n",
    variant("prc_m0_prefix", CUT_PREFIX),
    variant("prc_m0_cgetdata", CUT_CGETDATA),
    variant("prc_m0_cfxj", CUT_CFXJ),
]
sys.stdout.write("".join(out))
