//! DML/DDL 形态：语句改写 + 落库状态断言（每条一个全新内核，互不污染）。
//!
//! `setup` 是同一内核里先跑的前置语句（`;` 分隔，纯建数据用，不参与断言）；
//! `stmt` 是被测语句，走完整改写管线；`check` 读回落库状态。
//!
//! DDL 的类型写法复刻语料实际拼写（`character varying` / `numeric` / `date`）：
//! `[types]` 段是 Gauss 侧类型名 → DuckDB 的映射表，而语料导出的 DDL 已是
//! PG 规范拼写，DuckDB 原生可执行；把 DDL 翻译接进管线是 V18 的独立缺口，
//! 不在本表断言（编一条 `NUMBER(8)` 建表出来只会得到内核报错，不是形态证据）。

#![allow(dead_code)]

pub struct S {
    pub key: &'static str,
    pub why: &'static str,
    /// 前置语句（`;` 分隔；可为空串）
    pub setup: &'static str,
    pub stmt: &'static str,
    pub check: &'static str,
    pub want: &'static [&'static [&'static str]],
}

pub const STATEMENTS: &[S] = &[
    S {
        key: "head_insert",
        why: "INSERT … VALUES 指定列（语料 85 处）",
        setup: "",
        stmt: "INSERT INTO t_out (id, v, n) VALUES (10, 'ten', 10.5)",
        check: "SELECT id, v, n FROM t_out",
        want: &[&["10", "ten", "10.5"]],
    },
    S {
        key: "insert_values",
        why: "VALUES 多行批插",
        setup: "",
        stmt: "INSERT INTO t_out VALUES (1, 'a', 1.0), (2, 'b', 2.0)",
        check: "SELECT COUNT(*) FROM t_out",
        want: &[&["2"]],
    },
    S {
        key: "insert_select",
        why: "INSERT … SELECT（语料 51 处，E2 失败根因形态）",
        setup: "",
        stmt: "INSERT INTO t_out (id, v, n) SELECT id, fund, qty FROM t_item WHERE kind = 'BOND'",
        check: "SELECT id, v, n FROM t_out",
        want: &[&["3", "F01", "50"]],
    },
    S {
        key: "insert_select",
        why: "INSERT … SELECT 带 WHERE 与表达式列",
        setup: "",
        stmt: "INSERT INTO t_out (id, v, n) SELECT id, sec, amt * 0.5 FROM t_item WHERE id <= 2",
        check: "SELECT id, v, n FROM t_out ORDER BY id",
        want: &[&["1", "S001", "600"], &["2", "S002", "400"]],
    },
    S {
        key: "sql_rownum",
        why: "INSERT … SELECT 内层排序 + 外层 rownum = 1（语料主流首行形态）",
        setup: "",
        stmt: "INSERT INTO t_out (id, v) SELECT id, fund FROM (SELECT id, fund FROM t_item ORDER BY id) WHERE rownum = 1",
        check: "SELECT id, v FROM t_out",
        want: &[&["1", "F01"]],
    },
    S {
        key: "head_update",
        why: "UPDATE SET 数值列（语料 16 处）",
        setup: "INSERT INTO t_out VALUES (1, 'a', 1.0), (2, 'b', 2.0)",
        stmt: "UPDATE t_out SET n = 99.9 WHERE id = 1",
        check: "SELECT id, n FROM t_out ORDER BY id",
        want: &[&["1", "99.9"], &["2", "2"]],
    },
    S {
        key: "head_update",
        why: "UPDATE 表别名 + SET 限定前缀（patterns 剥离形态）",
        setup: "INSERT INTO t_out VALUES (1, 'a', 1.0), (2, 'b', 2.0)",
        stmt: "UPDATE t_out o SET o.v = 'upd' WHERE o.id = 2",
        check: "SELECT v FROM t_out WHERE id = 2",
        want: &[&["upd"]],
    },
    S {
        key: "head_delete",
        why: "DELETE 带条件（语料 9 处）",
        setup: "INSERT INTO t_out VALUES (1, 'a', 1.0), (2, 'b', 2.0)",
        stmt: "DELETE FROM t_out WHERE id = 1",
        check: "SELECT COUNT(*) FROM t_out",
        want: &[&["1"]],
    },
    S {
        key: "pl_dynamic_truncate",
        why: "TRUNCATE TABLE（动态 SQL 下发的日终清表形态，语料 3 处）",
        setup: "INSERT INTO t_out VALUES (1, 'a', 1.0), (2, 'b', 2.0)",
        stmt: "TRUNCATE TABLE t_out",
        check: "SELECT COUNT(*) FROM t_out",
        want: &[&["0"]],
    },
    S {
        key: "head_create_table",
        why: "语料 DDL 拼写建表（character varying/numeric/date，94 处）",
        setup: "",
        stmt: "CREATE TABLE t_new (a numeric(8,2), b character varying(50), c date, d numeric)",
        check: "SELECT COUNT(*) FROM t_new",
        want: &[&["0"]],
    },
    S {
        key: "head_create_table",
        why: "语料 DDL 建表后可写入（列宽/精度按 numeric 生效）",
        setup: "CREATE TABLE t_new2 (a numeric(18,4), b character varying(50))",
        stmt: "INSERT INTO t_new2 VALUES (1.25, 'x')",
        check: "SELECT a, b FROM t_new2",
        want: &[&["1.25", "x"]],
    },
    S {
        key: "head_create_view",
        why: "CREATE VIEW（语料 11 处）",
        setup: "",
        stmt: "CREATE VIEW v_stock AS SELECT id, fund, qty FROM t_item WHERE kind = 'STOCK'",
        check: "SELECT COUNT(*) FROM v_stock",
        want: &[&["3"]],
    },
    S {
        key: "ddl_create_sequence",
        why: "CREATE SEQUENCE（语料 2 处）",
        setup: "",
        stmt: "CREATE SEQUENCE seq_log START WITH 1 INCREMENT BY 1",
        check: "SELECT seq_log.nextval FROM sys_dummy",
        want: &[&["1"]],
    },
    S {
        key: "ddl_sequence_start_with",
        why: "START WITH 业务起点 + MINVALUE/MAXVALUE（语料逐字的 CACHE / 超 BIGINT MAXVALUE 由部署侧剥离，见 deploy_path_strips_corpus_sequence_options）",
        setup: "",
        stmt: "CREATE SEQUENCE seq_job START WITH 20001 INCREMENT BY 1 MINVALUE 20001 MAXVALUE 999999",
        check: "SELECT seq_job.nextval, seq_job.nextval FROM sys_dummy",
        // START WITH 生效（首个 20001），第二个按 INCREMENT BY 1 递增
        want: &[&["20001", "20002"]],
    },
    S {
        key: "pl_sequence_nextval",
        why: "seq.NEXTVAL 取号 → nextval('seq')（语料 12 处）",
        setup: "CREATE SEQUENCE seq_log START WITH 1 INCREMENT BY 1",
        stmt: "INSERT INTO t_out (id, v) VALUES (seq_log.nextval, 'x')",
        check: "SELECT id, v FROM t_out",
        want: &[&["1", "x"]],
    },
];
