#!/usr/bin/env python3
"""从考题语料抽取 SQL/PL 语句形态清单（脱敏：只出构造名 + 计数 + 文件代号）。

输出：
  <清单路径>（默认 /tmp/corpus_forms.toml；提交版为 config/corpus_forms.toml）
      —— 构造 → 命中次数、涉及文件代号、零命中构造清单
  /tmp/corpus_forms_map.json  —— 代号↔相对路径映射（留本地，不入库）
用法：python3 scripts/extract_corpus_forms.py <corpus_sql_dir> [清单输出路径]
"""
import hashlib
import json
import re
import sys
from pathlib import Path

SQL = Path(sys.argv[1] if len(sys.argv) > 1 else "sql")
OUT = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("/tmp/corpus_forms.toml")

# 构造检测：key → 正则（大小写不敏感，作用于去注释后的语句文本）
C = {
    # --- 语句头 ---
    "head_select": r"(?im)^\s*SELECT\b",
    "head_insert": r"(?im)^\s*INSERT\s+INTO\b",
    "head_update": r"(?im)^\s*UPDATE\b",
    "head_delete": r"(?im)^\s*DELETE\b",
    "head_merge": r"(?im)^\s*MERGE\s+INTO\b",
    "head_create_table": r"(?im)^\s*CREATE\s+TABLE\b",
    "head_create_view": r"(?im)^\s*CREATE\s+(OR\s+REPLACE\s+)?VIEW\b",
    "head_create_index": r"(?im)^\s*CREATE\s+(OR\s+REPLACE\s+)?(UNIQUE\s+)?INDEX\b",
    "head_alter": r"(?im)^\s*ALTER\b",
    "head_comment": r"(?im)^\s*COMMENT\s+ON\b",
    "head_truncate": r"(?im)^\s*(TRUNCATE\b|ALTER\s+TABLE\s+\S+\s+TRUNCATE\s+PARTITION)",
    # --- 查询形态 ---
    "insert_select": r"(?is)INSERT\s+INTO[\s\S]*?\bSELECT\b",
    "insert_values": r"(?is)INSERT\s+INTO[\s\S]*?VALUES\s*\(",
    "select_into": r"(?is)\bSELECT\b[\s\S]*?\bINTO\b[\s\S]*?\bFROM\b",
    "from_multi_table": r"(?is)\bFROM\s+[\w$.]+(\s+[a-zA-Z_]\w*)?\s*,\s*[\w$.]+",
    "join_implicit_where": r"(?is)\bFROM\s+[\w$.]+(\s+[a-zA-Z_]\w*)?\s*,\s*[\w$.]+[\s\S]{0,400}?\bWHERE\b",
    "join_explicit": r"(?i)\bJOIN\b",
    "join_left": r"(?i)\bLEFT\s+(OUTER\s+)?JOIN\b",
    "join_right": r"(?i)\bRIGHT\s+(OUTER\s+)?JOIN\b",
    "join_full": r"(?i)\bFULL\s+(OUTER\s+)?JOIN\b",
    "oracle_outer_join_marker": r"\(\s*\+\s*\)",
    "union_all": r"(?i)\bUNION\s+ALL\b",
    "union_plain": r"(?i)\bUNION\b(?!\s+ALL)",
    "intersect_except_minus": r"(?i)\b(INTERSECT|EXCEPT|MINUS)\b",
    "subquery_in": r"(?i)\bIN\s*\(\s*SELECT\b",
    "subquery_scalar": r"(?i)\(\s*SELECT\b",
    "exists": r"(?i)\b(?:NOT\s+)?EXISTS\s*\(",
    "between": r"(?i)\b(?:NOT\s+)?BETWEEN\b",
    "distinct": r"(?i)\bSELECT\s+DISTINCT\b",
    "group_by": r"(?i)\bGROUP\s+BY\b",
    "having": r"(?i)\bHAVING\b",
    "order_by": r"(?i)\bORDER\s+BY\b",
    "connect_by": r"(?i)\bCONNECT\s+BY\b",
    "ddl_sequence_start_with": r"(?i)\bSTART\s+WITH\b",
    "in_literal_list": r"(?i)\bIN\s*\(\s*'",
    "alias_without_as": r"(?i)\b(COUNT|SUM|MAX|MIN)\s*\([^)]*\)\s+[A-Za-z_]\w*\s*(?:,|$)",
    "group_by_expression": r"(?i)\bGROUP\s+BY\s+[^;]*\(",
    "call_within_call": r"(?i)[\w$]+\s*\([^()]*[\w$]+\s*\(",
    "not_compound": r"(?i)\bNOT\s*\(",
    "pl_package_call": r"(?i)\b[a-z_][\w$]*\.[a-z_][\w$]*\s*\(",
    "pl_assign": r":=",
    "aggregate_sum": r"(?i)\bSUM\s*\(",
    "aggregate_count": r"(?i)\bCOUNT\s*\(",
    "aggregate_max_min": r"(?i)\b(MAX|MIN)\s*\(",
    "case_when": r"(?i)\bCASE\s+(?:WHEN|[\w$])",
    "derived_table": r"(?i)\bFROM\s*\(\s*SELECT\b",
    # --- 表达式/函数面 ---
    "fn_decode": r"(?i)\bDECODE\s*\(",
    "fn_nvl": r"(?i)\bNVL2?\s*\(",
    "fn_subinstr": r"(?i)\b(SUBSTR|INSTR)\s*\(",
    "fn_lpad_rpad": r"(?i)\b(LPAD|RPAD)\s*\(",
    "fn_tochar_todate": r"(?i)\b(TO_CHAR|TO_DATE)\s*\(",
    "fn_trunc_round": r"(?i)\b(TRUNC|ROUND)\s*\(",
    "fn_date_arith": r"(?i)\b(ADD_MONTHS|MONTHS_BETWEEN|LAST_DAY|NEXT_DAY|SYSDATE|SYSTIMESTAMP)\b",
    "fn_sign_abs": r"(?i)\b(SIGN|ABS)\s*\(",
    "fn_length_trim": r"(?i)\b(LENGTH|LTRIM|RTRIM|TRIM)\s*\(",
    "fn_replace_upper_lower": r"(?i)\b(REPLACE|UPPER|LOWER)\s*\(",
    "fn_mod": r"(?i)\bMOD\s*\(",
    "custom_intervaltonum": r"(?i)\bINTERVALTONUM\b",
    "string_concat": r"\|\|",
    "numeric_overflow_cast": r"(?i)\bCAST\s*\(",
    # --- PL 面 ---
    "pl_package": r"(?i)^\s*CREATE\s+(OR\s+REPLACE\s+)?PACKAGE\b",
    "pl_package_body": r"(?i)^\s*CREATE\s+(OR\s+REPLACE\s+)?PACKAGE\s+BODY\b",
    "pl_procedure": r"(?i)\bPROCEDURE\s+\w+",
    "pl_function": r"(?i)\bFUNCTION\s+\w+",
    "pl_type_attr": r"(?i)%\s*(TYPE|ROWTYPE)\b",
    "pl_cursor_decl": r"(?i)\bCURSOR\s+\w+",
    "pl_open_fetch": r"(?i)\b(OPEN\s+\w+|FETCH\s+\w+\s+INTO)\b",
    "pl_notfound_found": r"(?i)%\s*(NOTFOUND|FOUND)\b",
    "pl_loop": r"(?i)^\s*LOOP\b",
    "pl_for_loop": r"(?i)\bFOR\s+\w+\s+IN\b",
    "pl_while": r"(?i)\bWHILE\b",
    "pl_exit_when": r"(?i)\bEXIT\s+WHEN\b",
    "pl_continue": r"(?i)\bCONTINUE\s+WHEN\b",
    "pl_if": r"(?i)^\s*IF\b",
    "pl_elsif": r"(?i)\bELSIF\b",
    "pl_case": r"(?i)^\s*CASE\b",
    "pl_exception_when_others": r"(?i)\bWHEN\s+OTHERS\b",
    # 具名处理器：`WHEN <异常名> THEN`。旧式 `EXCEPTION WHEN [A-Za-z_]` 把 WHEN
    # OTHERS 一并计入（与上一键重复），此处排除 OTHERS，只统计内建具名异常。
    "pl_named_exception": r"(?i)\bWHEN\s+(?!OTHERS\b)[A-Za-z_]\w*\s+THEN",
    # 语料只出现裸 `RAISE;`（处理器末尾重抛）；`RAISE <具名>` 与
    # RAISE_APPLICATION_ERROR 均零命中，故不计入本键。
    "pl_raise": r"(?i)\bRAISE\s*;",
    "pl_pragema_exception_init": r"(?i)\bPRAGMA\s+EXCEPTION_INIT\b",
    "pl_sqlerrm_sqlcode": r"(?i)\b(SQLERRM|SQLCODE)\b",
    "pl_commit_rollback": r"(?i)\b(COMMIT|ROLLBACK)\b",
    "pl_savepoint": r"(?i)\bSAVEPOINT\b",
    "pl_execute_immediate": r"(?i)\bEXECUTE\s+IMMEDIATE\b",
    "pl_bulk_collect": r"(?i)\bBULK\s+COLLECT\b",
    "pl_forall": r"(?i)\bFORALL\b",

    "pl_dbms": r"(?i)\bDBMS_[A-Z_]+\.",
    "pl_sequence_nextval": r"(?i)\b\w+\.NEXTVAL\b",
    "pl_date_switch_assign": r"(?i)\bTO_DATE\b",
    "pl_open_for": r"(?i)\bOPEN\s+\w+\s+FOR\b",
    "pl_isopen": r"(?i)%\s*ISOPEN\b",
    "pl_param_mode": r"(?i)\b(IN\s+OUT|OUT)\b",
    # 形参默认值：以 Oracle 类型名锚定。裸 `DEFAULT` 会把 DDL 列默认值
    # （`character varying(1) default`，Gauss 原生类型写法）一并计入，掩盖真实形态数。
    "pl_param_default": r"(?i)\b(?:NUMBER|VARCHAR2|VARCHAR|DATE|INTEGER|CLOB|DECIMAL)\s*(?:\([\d,]+\))?\s+DEFAULT\b",
    "pl_varchar2": r"(?i)\bVARCHAR2\s*\(",
    "pl_number_type": r"(?i)\bNUMBER\s*\(",
    "pl_date_type": r"(?i)\bDATE\b",
    # 异常常量声明是「名字 EXCEPTION;」（`ex EXCEPTION;`），不是 `EXCEPTION <名字>;`
    "pl_exception_decl": r"(?i)\b\w+\s+EXCEPTION\s*;",
    "ddl_create_sequence": r"(?i)\bCREATE\s+SEQUENCE\b",
    "ddl_drop_table": r"(?i)\bDROP\s+TABLE\b",
    "ddl_alter_add": r"(?i)\bALTER\s+TABLE\b[\s\S]{0,80}\bADD\b",
    "sql_with_cte": r"(?im)^\s*WITH\b",
    "sql_rownum": r"(?i)\bROWNUM\b",
    "fn_to_number": r"(?i)\bTO_NUMBER\s*\(",
    "fn_numtointerval": r"(?i)\b(NUMTODSINTERVAL|NUMTOYMINTERVAL)\b",
    "fn_greatest_least": r"(?i)\b(GREATEST|LEAST)\s*\(",
    # 计数走 count_empty_literals（保留字面量的文本），不是本正则
    "literal_empty_string_null": r"''",
    # --- 拒绝面（应零命中；命中即 PR 需说明） ---
    "reject_distribute_by": r"(?i)\bDISTRIBUTE\s+BY\b",
    "reject_rowtype_in_sql": r"(?i)\bINSERT\s+[^;]*%ROWTYPE",
    "reject_partition_ddl": r"(?i)\bPARTITION\s+BY\s+(RANGE|LIST|HASH)\b",
    "analytic_over": r"(?i)\bOVER\s*\(",
    "analytic_lead_lag": r"(?i)\b(LEAD|LAG)\s*\(",
    "analytic_row_number": r"(?i)\b(ROW_NUMBER|RANK|DENSE_RANK|SUM)\s*\([^)]*\)\s*OVER\b",
    "pl_goto": r"(?i)\bGOTO\s+\w+",
    "pl_label": r"<<\s*\w+\s*>>",
    "pl_dynamic_truncate": r"(?is)EXECUTE\s+IMMEDIATE\s+'[^']*truncate",
    "reject_dbms_package": r"(?i)\bDBMS_\w+\.\w+",
    "reject_autonomous": r"(?i)\bAUTONOMOUS_TRANSACTION\b",
}

# 少数构造只在**未去字面量**的原文上才可见（动态 SQL 的表名在字符串里）。
RAW_ONLY = {"pl_dynamic_truncate"}

# `''` 本身是一个完整字面量，去字面量后不可见；但 A 模式 `'' ≡ NULL` 是口径项，
# 必须在**去注释、留字面量**的文本上计数（否则清单会谎报"语料零命中"）。
KEEP_LIT = {"literal_empty_string_null"}

BLOCK = re.compile(r"/\*.*?\*/", re.S)
LINE = re.compile(r"--[^\n]*")
LIT = re.compile(r"'(?:[^']|'')*'")


def strip(text: str) -> str:
    text = BLOCK.sub(" ", text)
    text = LINE.sub(" ", text)
    text = LIT.sub("'LIT'", text)
    return text


def strip_comments(text: str) -> str:
    return LINE.sub(" ", BLOCK.sub(" ", text))


def count_empty_literals(text: str) -> int:
    """完整字面量扫描口径：只数空串 `''`。

    直接 `''` 正则会把 `'it''s'` 里的转义引号对误计为空串，故按整串匹配再判空。
    """
    n = 0
    for m in LIT.finditer(text):
        if m.group(0)[1:-1].replace("''", "'") == "":
            n += 1
    return n


def sha8(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()[:12]


files = sorted(SQL.rglob("*.sql"))
code_map, counts, where = {}, {}, {}
stub = [f for f in files if f.name.upper().startswith("STUB_")]
for idx, f in enumerate(files, start=1):
    code = f"F{idx:03d}"
    code_map[code] = {
        "path": str(f),
        "kind": "stub" if f in stub else "corpus",
        "sha256_12": sha8(f),
    }
    raw = f.read_text(encoding="utf-8", errors="replace")
    text = strip(raw)
    keep = strip_comments(raw)
    for key, pat in C.items():
        if key in RAW_ONLY:
            n = len(re.findall(pat, raw, re.M))
        elif key in KEEP_LIT:
            n = count_empty_literals(keep)
        else:
            n = len(re.findall(pat, text, re.M))
        if n:
            counts[key] = counts.get(key, 0) + n
            where.setdefault(key, []).append(code)

out = [
    "# 语料语句形态清单（脱敏）",
    "# 由 scripts/extract_corpus_forms.py 生成；仅含构造名、命中次数与文件代号。",
    "# 代号 ↔ 路径映射不入库（本地语料含远端库凭据）。",
    "# 用途：spinduck/tests/corpus_sql_forms.rs 按本清单逐构造断言（覆盖门禁）。",
    "",
    'corpus_root = "本地语料目录（不入库：含远端库凭据）"',
    f"files = {len(files)}",
    f'files_stub_authored = {len(stub)}',
    "",
    "[constructs]",
]
for key in sorted(counts):
    out.append(f"{key} = {counts[key]}")
out.append("")
out.append("[construct_files]")
for key in sorted(counts):
    out.append(f"{key} = [{', '.join(where[key][:12])}]" + ("…" if len(where[key]) > 12 else ""))
out.append("")
out.append("# 语料零命中构造：拒绝面/未覆盖面，清单本身即证据")
out.append("[zero_hits]")
out.append("keys = [" + ", ".join(f'"{k}"' for k in sorted(set(C) - set(counts))) + "]")
out_text = "\n".join(out) + "\n"
OUT.write_text(out_text)
Path("/tmp/corpus_forms_map.json").write_text(json.dumps(code_map, ensure_ascii=False, indent=1))
print(f"files={len(files)} stubs={len(stub)} constructs_with_hits={len(counts)}")
for key in sorted(counts, key=lambda k: -counts[k]):
    print(f"  {counts[key]:6d}  {key}")
print("零命中构造：", ", ".join(sorted(set(C) - set(counts))))
