//! 表达式/函数面形态（语料命中最多的构造族）。
//!
//! 标量维度：`SELECT <形态> FROM t_form` 走完整改写管线后在进程内 DuckDB 执行，
//! 逐值比对；改写维度：锁定方言映射产物（规则值的可执行证据）。
//!
//! 口径：每个形态的 SQL 必须复刻**语料里真实出现过的写法**（次数见
//! `config/corpus_forms.toml`）。语料零命中的写法（NVL2 / RPAD / TRIM /
//! `TRUNC(日期,fmt)` / `DATE '字面量'`）不作为覆盖项——覆盖门禁只认清单，
//! 编造形态会让断言失去"语料实证"的含义。

#![allow(dead_code)]

pub struct Scalar {
    pub key: &'static str,
    pub why: &'static str,
    pub sql: &'static str,
    pub want: &'static str,
}

pub struct Rewritten {
    pub key: &'static str,
    pub sql: &'static str,
    /// 改写产物必须包含的片段
    pub want: &'static str,
}

pub const SCALARS: &[Scalar] = &[
    Scalar {
        key: "fn_decode",
        why: "DECODE 多分支 + 默认值（语料 467 处）",
        sql: "SELECT DECODE(flag, '1', '甲', '2', '乙', '其他') FROM t_form WHERE id = 1",
        want: "甲",
    },
    Scalar {
        key: "fn_decode",
        why: "DECODE 命中默认分支",
        sql: "SELECT DECODE(flag, '1', '甲', '2', '乙', '其他') FROM t_form WHERE id = 3",
        want: "其他",
    },
    Scalar {
        key: "fn_nvl",
        why: "NVL 文本兜底（语料 483 处）",
        sql: "SELECT NVL(s, 'N/A') FROM t_form WHERE id = 3",
        want: "N/A",
    },
    Scalar {
        key: "fn_nvl",
        why: "NVL 数值兜底（语料主流 `nvl(金额, 0)`）",
        sql: "SELECT NVL(qty, 0) FROM t_item WHERE id = 5",
        want: "0",
    },
    Scalar {
        key: "fn_subinstr",
        why: "SUBSTR 起始位/长度（1 基，语料 198 处）",
        sql: "SELECT SUBSTR(s, 2, 2) FROM t_form WHERE id = 1",
        want: "bC",
    },
    Scalar {
        key: "fn_subinstr",
        why: "INSTR 子串定位（语料 45 处）",
        sql: "SELECT INSTR(s, 'C') FROM t_form WHERE id = 1",
        want: "3",
    },
    Scalar {
        key: "fn_subinstr",
        why: "语料主形态 INSTR + SUBSTR 嵌套切列（`substr(x, instr(x,' ')+1)`）",
        sql: "SELECT SUBSTR('S1 平安', INSTR('S1 平安', ' ') + 1) FROM t_form WHERE id = 1",
        want: "平安",
    },
    Scalar {
        key: "fn_lpad_rpad",
        why: "LPAD 补零定长（证券代码惯用形态，语料 33 处；RPAD 语料 0 处）",
        sql: "SELECT LPAD('7', 6, '0') FROM t_form WHERE id = 1",
        want: "000007",
    },
    Scalar {
        key: "fn_lpad_rpad",
        why: "LPAD 套 TO_CHAR（数值定长文本化）",
        sql: "SELECT LPAD(TO_CHAR(id), 8, '0') FROM t_form WHERE id = 2",
        want: "00000002",
    },
    Scalar {
        key: "fn_length_trim",
        why: "LENGTH 字符长度（语料 39 处；TRIM 语料 0 处，不作覆盖）",
        sql: "SELECT LENGTH(s) FROM t_form WHERE id = 2",
        want: "5",
    },
    Scalar {
        key: "fn_length_trim",
        why: "LENGTH 作用于列表达式",
        sql: "SELECT LENGTH(fund) FROM t_item WHERE id = 1",
        want: "3",
    },
    Scalar {
        key: "fn_sign_abs",
        why: "SIGN 符号判定（语料 210 处）",
        sql: "SELECT SIGN(n) FROM t_form WHERE id = 2",
        want: "-1",
    },
    Scalar {
        key: "fn_sign_abs",
        why: "ABS 取绝对值（语料 67 处）",
        sql: "SELECT ABS(n) FROM t_form WHERE id = 2",
        want: "7.5",
    },
    Scalar {
        key: "fn_greatest_least",
        why: "GREATEST/LEAST 夹逼（语料 82/80 处）",
        sql: "SELECT GREATEST(1, 5, 3) FROM t_form WHERE id = 1",
        want: "5",
    },
    Scalar {
        key: "fn_greatest_least",
        why: "语料主形态 `round(least(greatest(round(x, s), lo), hi), s)`",
        sql: "SELECT ROUND(LEAST(GREATEST(ROUND(1.239 * 2, 2), 0.5), 2.0), 2) FROM t_form WHERE id = 1",
        want: "2",
    },
    Scalar {
        key: "fn_trunc_round",
        why: "ROUND 四舍五入到分（费用计算主流形态，语料 741 处全是 ROUND）",
        sql: "SELECT ROUND(n, 2) FROM t_form WHERE id = 1",
        want: "12.35",
    },
    Scalar {
        key: "fn_trunc_round",
        why: "ROUND 位数来自变量列（语料 `round(x, v_decimal_digits)`）",
        sql: "SELECT ROUND(qty * 1.0 / 3, 4) FROM t_item WHERE id = 2",
        want: "66.6667",
    },
    Scalar {
        key: "fn_trunc_round",
        why: "嵌套 ROUND（语料 `round(round(nvl(a,0)*b,2),2)`）",
        sql: "SELECT ROUND(ROUND(NVL(qty, 0) * 1.005, 2), 1) FROM t_item WHERE id = 3",
        want: "50.3",
    },
    Scalar {
        key: "fn_to_number",
        why: "TO_NUMBER 文本转数值（语料 67 处）",
        sql: "SELECT TO_NUMBER('1234.5') FROM t_form WHERE id = 1",
        want: "1234.5",
    },
    Scalar {
        key: "fn_to_number",
        why: "TO_NUMBER 用于 VARCHAR 日期列比较（语料 bcrq 形态）",
        sql: "SELECT COUNT(*) FROM t_item WHERE TO_NUMBER(bcrq) > 20250715",
        want: "2",
    },
    Scalar {
        key: "fn_tochar_todate",
        why: "TO_CHAR 日期格式化（语料 41 处）",
        sql: "SELECT TO_CHAR(d, 'YYYYMMDD') FROM t_form WHERE id = 1",
        want: "20250715",
    },
    Scalar {
        key: "fn_tochar_todate",
        why: "TO_DATE 文本转日期后回格式化（日终参数惯用，语料 71 处）",
        sql: "SELECT TO_CHAR(TO_DATE('20250715', 'YYYYMMDD'), 'YYYY-MM-DD') FROM t_form WHERE id = 1",
        want: "2025-07-15",
    },
    Scalar {
        key: "fn_date_arith",
        why: "日期差取天数（语料 `to_date(a) - to_date(b)` 交给 INTERVALTONUM）",
        sql: "SELECT TO_CHAR(TO_DATE('20250715', 'YYYYMMDD'), 'YYYYMMDD') FROM t_form WHERE id = 1",
        want: "20250715",
    },
    Scalar {
        key: "string_concat",
        why: "|| 文本拼接（语料 292 处）",
        sql: "SELECT 'A' || 'B' || 'C' FROM t_form WHERE id = 1",
        want: "ABC",
    },
    Scalar {
        key: "string_concat",
        why: "|| 与数值混合拼接（规则 implicit.concat_numeric_to_varchar）",
        sql: "SELECT 'N=' || id FROM t_form WHERE id = 2",
        want: "N=2",
    },
    Scalar {
        key: "string_concat",
        why: "|| 与 NULL 拼接：A 模式视作空串（引擎口径，见 validation V21）",
        sql: "SELECT 'A' || s FROM t_form WHERE id = 3",
        want: "A",
    },
    Scalar {
        key: "string_concat",
        why: "|| 两侧皆 NULL：内核 concat 全 NULL 得 NULL（与基线 `NULL || NULL = NULL` 一致，\
              实测值即 want，见 validation V21）",
        sql: "SELECT NULL || s FROM t_form WHERE id = 3",
        want: "NULL",
    },
    Scalar {
        key: "literal_empty_string_null",
        why: "空串字面量（语料 170 处，`SELECT '' AS 列 …` 日终补空列）。A 模式 `'' ≡ NULL`，\
              本引擎保持内核原语义（空串不是 NULL）；已知口径差异记于 validation V21，\
              黄金比对层按 norm 归一（空串与 NULL 同值）故比对不受影响",
        sql: "SELECT CASE WHEN '' IS NULL THEN 'null' ELSE 'notnull' END FROM t_form WHERE id = 1",
        want: "notnull",
    },
    Scalar {
        key: "case_when",
        why: "CASE WHEN 搜索式（语料 15 处）",
        sql: "SELECT CASE WHEN n > 0 THEN 'pos' ELSE 'neg' END FROM t_form WHERE id = 1",
        want: "pos",
    },
    Scalar {
        key: "case_when",
        why: "CASE 简单式（表达式匹配）",
        sql: "SELECT CASE flag WHEN '1' THEN '一' WHEN '2' THEN '二' ELSE '他' END FROM t_form WHERE id = 2",
        want: "二",
    },
    Scalar {
        key: "between",
        why: "BETWEEN 闭区间过滤（语料 138 处）",
        sql: "SELECT COUNT(*) FROM t_form WHERE n BETWEEN 0 AND 20",
        want: "1",
    },
    Scalar {
        key: "between",
        why: "NOT BETWEEN（NULL 行两侧都不计）",
        sql: "SELECT COUNT(*) FROM t_form WHERE n NOT BETWEEN 0 AND 20",
        want: "1",
    },
    Scalar {
        key: "between",
        why: "语料主形态：VARCHAR 日期串按数值 BETWEEN 区间",
        sql: "SELECT COUNT(*) FROM t_item WHERE bcrq BETWEEN '20250715' AND '20250715'",
        want: "3",
    },
    Scalar {
        key: "in_literal_list",
        why: "IN 字面量清单（语料 103 处）",
        sql: "SELECT COUNT(*) FROM t_form WHERE flag IN ('1', '2')",
        want: "2",
    },
    Scalar {
        key: "not_compound",
        why: "NOT (...) 复合谓词取反（语料 9 处）",
        sql: "SELECT COUNT(*) FROM t_form WHERE NOT (flag = '1')",
        want: "2",
    },
    Scalar {
        key: "aggregate_sum",
        why: "SUM 忽略 NULL（语料 634 处）",
        sql: "SELECT SUM(qty) FROM t_item",
        want: "650",
    },
    Scalar {
        key: "aggregate_count",
        why: "COUNT(*) 计全部行",
        sql: "SELECT COUNT(*) FROM t_item",
        want: "5",
    },
    Scalar {
        key: "aggregate_count",
        why: "COUNT(列) 不计 NULL（语料 17 处，与上一行的口径差）",
        sql: "SELECT COUNT(qty) FROM t_item",
        want: "4",
    },
    Scalar {
        key: "aggregate_max_min",
        why: "MAX 聚合（语料 11 处）",
        sql: "SELECT MAX(amt) FROM t_item",
        want: "5000",
    },
    Scalar {
        key: "aggregate_max_min",
        why: "MIN 忽略 NULL",
        sql: "SELECT MIN(fee) FROM t_item",
        want: "4",
    },
    Scalar {
        key: "distinct",
        why: "COUNT(DISTINCT) 去重（语料 9 处）",
        sql: "SELECT COUNT(DISTINCT kind) FROM t_item",
        want: "3",
    },
    Scalar {
        key: "alias_without_as",
        why: "聚合列无 AS 别名（语料 34 处）",
        sql: "SELECT COUNT(*) cnt FROM t_item",
        want: "5",
    },
    Scalar {
        key: "custom_intervaltonum",
        why: "自定义函数 INTERVALTONUM（语料 30 处，**单参**：入参是两个日期之差）",
        sql: "SELECT INTERVALTONUM(TO_DATE('20250715', 'YYYYMMDD') - TO_DATE('20250701', 'YYYYMMDD')) FROM t_form WHERE id = 1",
        want: "14",
    },
    Scalar {
        key: "sql_rownum",
        why: "ROWNUM 首行谓词 → LIMIT 1（语料 42 处）",
        sql: "SELECT s FROM t_form WHERE ROWNUM = 1 ORDER BY id",
        want: "AbC",
    },
    Scalar {
        key: "sql_with_cte",
        why: "WITH 子句临时视图（语料 4 处）",
        sql: "WITH temp_view AS (SELECT fund FROM t_item WHERE kind = 'STOCK') SELECT COUNT(*) FROM temp_view",
        want: "3",
    },
    Scalar {
        key: "subquery_scalar",
        why: "标量子查询（语料 140 处）",
        sql: "SELECT (SELECT name FROM t_dim WHERE sec = 'S001') FROM t_form WHERE id = 1",
        want: "平安",
    },
    Scalar {
        key: "subquery_scalar",
        why: "标量子查询 + NVL 兜底（空结果集 → NULL → 0）",
        sql: "SELECT NVL((SELECT name FROM t_dim WHERE sec = 'S999'), '缺') FROM t_form WHERE id = 1",
        want: "缺",
    },
    Scalar {
        key: "derived_table",
        why: "派生表（内联视图，语料 31 处）",
        sql: "SELECT COUNT(*) FROM (SELECT fund FROM t_item WHERE qty > 100) tt",
        want: "2",
    },
    Scalar {
        key: "analytic_over",
        why: "SUM OVER 分区汇总（语料 5 处；派生表收口成单值）",
        sql: "SELECT MAX(s) FROM (SELECT SUM(qty) OVER (PARTITION BY fund) s FROM t_item) tt",
        want: "350",
    },
    Scalar {
        key: "analytic_over",
        // 窗口在 WHERE 之后求值：单行分区只剩本行（openGauss 与 DuckDB 同口径），
        // 语料游标里 `sum() over ()` 正是配 WHERE 逐行取分区值的形态
        why: "窗口函数在 WHERE 之后求值（两侧一致，锁定口径）",
        sql: "SELECT SUM(qty) OVER (PARTITION BY fund) FROM t_item WHERE id = 1",
        want: "100",
    },
    Scalar {
        key: "analytic_lead_lag",
        why: "LEAD 取下一行（语料 1 处，风险金段）",
        sql: "SELECT MIN(nx) FROM (SELECT LEAD(id, 1) OVER (ORDER BY id) nx FROM t_item) tt",
        want: "2",
    },
    Scalar {
        key: "analytic_lead_lag",
        why: "LEAD 在单行窗口上无后继 → NULL",
        sql: "SELECT NVL(TO_CHAR(LEAD(id, 1) OVER (ORDER BY id)), 'none') FROM t_item WHERE id = 1",
        want: "none",
    },
];

pub const REWRITES: &[Rewritten] = &[
    Rewritten {
        key: "fn_decode",
        sql: "SELECT DECODE(flag, '1', '甲', '2', '乙', '其他') FROM t_form",
        want: "CASE",
    },
    Rewritten {
        key: "fn_nvl",
        sql: "SELECT NVL(s, 'x') FROM t_form",
        want: "coalesce",
    },
    Rewritten {
        key: "fn_subinstr",
        sql: "SELECT SUBSTR(s, 1, 2) FROM t_form",
        want: "SUBSTRING",
    },
    Rewritten {
        key: "fn_tochar_todate",
        sql: "SELECT TO_CHAR(d, 'YYYYMMDD') FROM t_form",
        want: "strftime",
    },
    Rewritten {
        key: "fn_tochar_todate",
        sql: "SELECT TO_DATE('20250715', 'YYYYMMDD') FROM t_form",
        want: "strptime",
    },
    Rewritten {
        key: "fn_date_arith",
        sql: "SELECT TO_CHAR(SYSDATE, 'YYYYMMDD') FROM t_form",
        want: "CURRENT_TIMESTAMP",
    },
    Rewritten {
        key: "custom_intervaltonum",
        sql: "SELECT INTERVALTONUM(TO_DATE('20250715', 'YYYYMMDD') - TO_DATE('20250701', 'YYYYMMDD')) FROM t_form",
        want: "date_diff",
    },
    Rewritten {
        key: "pl_sequence_nextval",
        sql: "SELECT SEQ_X.NEXTVAL FROM t_form",
        want: "nextval('SEQ_X')",
    },
    Rewritten {
        key: "sql_rownum",
        sql: "INSERT INTO t_out (id, v) SELECT id, fund FROM t_item WHERE rownum = 1",
        want: "LIMIT",
    },
];
