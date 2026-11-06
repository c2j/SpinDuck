//! PL 过程/包形态：游标生命周期、事务、动态 SQL、包与 OUT 参（语料过程面）。
//!
//! 三个断言维度按需组合：`want`（调用后内核表状态）、`want_out`（OUT 参回值）、
//! `want` 为空数组且 `out_param` 为空则视为该维度不适用。

#![allow(dead_code)]

pub struct R {
    pub key: &'static str,
    pub why: &'static str,
    /// 例程源码（可含多条 CREATE 语句：包声明 + 包体 + 调用壳）
    pub routine: &'static str,
    pub call: &'static str,
    pub args: &'static [&'static str],
    /// 调用后读取的内核状态（空串 = 不读表）
    pub check: &'static str,
    pub want: &'static [&'static [&'static str]],
    /// OUT 参名（空串 = 无）
    pub out_param: &'static str,
    pub want_out: &'static str,
}

pub const ROUTINES: &[R] = &[
    R {
        key: "pl_procedure",
        why: "过程声明 + DML 写入（语料 46 处）",
        routine: "CREATE OR REPLACE PROCEDURE p_ins IS \
                  BEGIN INSERT INTO t_out VALUES (1, 'one', 1.0); END;",
        call: "p_ins",
        args: &[],
        check: "SELECT id, v, n FROM t_out",
        want: &[&["1", "one", "1"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_commit_rollback",
        why: "COMMIT 落定（语料 65 处）",
        routine: "CREATE OR REPLACE PROCEDURE p_commit IS \
                  BEGIN \
                    INSERT INTO t_out VALUES (1, 'a', 1); \
                    COMMIT; \
                    INSERT INTO t_out VALUES (2, 'b', 2); \
                  END;",
        call: "p_commit",
        args: &[],
        check: "SELECT COUNT(*) FROM t_out",
        want: &[&["2"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_commit_rollback",
        why: "ROLLBACK 撤销段内写入",
        routine: "CREATE OR REPLACE PROCEDURE p_rollback IS \
                  BEGIN \
                    INSERT INTO t_out VALUES (1, 'a', 1); \
                    ROLLBACK; \
                  END;",
        call: "p_rollback",
        args: &[],
        check: "SELECT COUNT(*) FROM t_out",
        want: &[&["0"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_cursor_decl",
        why: "游标声明 + OPEN/FETCH 循环聚合（语料 16/34 处）",
        routine: "CREATE OR REPLACE PROCEDURE p_cur(o OUT VARCHAR2) IS \
                  CURSOR c IS SELECT fund, amt FROM t_item WHERE kind = 'STOCK' ORDER BY id; \
                  v_f VARCHAR2(20); v_a DECIMAL(18,2); s DECIMAL(18,2) := 0; n NUMBER := 0; \
                  BEGIN \
                    OPEN c; \
                    LOOP \
                      FETCH c INTO v_f, v_a; \
                      EXIT WHEN c%NOTFOUND; \
                      s := s + v_a; \
                      n := n + 1; \
                    END LOOP; \
                    CLOSE c; \
                    o := 'rows=' || n || ',amt=' || s; \
                  END;",
        call: "p_cur",
        args: &["null"],
        check: "",
        want: &[],
        out_param: "o",
        want_out: "rows=3,amt=2900",
    },
    R {
        key: "pl_open_fetch",
        why: "游标 FETCH 结果写表",
        routine: "CREATE OR REPLACE PROCEDURE p_fetch IS \
                  CURSOR c IS SELECT id, fund FROM t_item WHERE qty > 150 ORDER BY id; \
                  v_id NUMBER; v_f VARCHAR2(20); \
                  BEGIN \
                    OPEN c; \
                    LOOP \
                      FETCH c INTO v_id, v_f; \
                      EXIT WHEN c%NOTFOUND; \
                      INSERT INTO t_out VALUES (v_id, v_f, 0); \
                    END LOOP; \
                    CLOSE c; \
                  END;",
        call: "p_fetch",
        args: &[],
        check: "SELECT id, v FROM t_out ORDER BY id",
        want: &[&["2", "F01"], &["4", "F02"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_exit_when",
        why: "EXIT WHEN 组合布尔条件（语料 17 处）",
        routine: "CREATE OR REPLACE PROCEDURE p_exit(o OUT VARCHAR2) IS \
                  i NUMBER := 0; \
                  BEGIN \
                    LOOP \
                      i := i + 1; \
                      EXIT WHEN i = 3 OR i > 10; \
                    END LOOP; \
                    o := i; \
                  END;",
        call: "p_exit",
        args: &["null"],
        check: "",
        want: &[],
        out_param: "o",
        want_out: "3",
    },
    R {
        key: "pl_notfound_found",
        why: "空结果集上 %NOTFOUND 立即为真",
        routine: "CREATE OR REPLACE PROCEDURE p_nf(o OUT VARCHAR2) IS \
                  CURSOR c IS SELECT id FROM t_item WHERE kind = 'NONE'; \
                  v NUMBER; \
                  BEGIN \
                    OPEN c; \
                    FETCH c INTO v; \
                    IF c%NOTFOUND THEN o := 'empty'; ELSE o := 'row'; END IF; \
                    CLOSE c; \
                  END;",
        call: "p_nf",
        args: &["null"],
        check: "",
        want: &[],
        out_param: "o",
        want_out: "empty",
    },
    R {
        key: "pl_execute_immediate",
        why: "EXECUTE IMMEDIATE 动态 DML（语料 3 处）",
        routine: "CREATE OR REPLACE PROCEDURE p_dyn IS \
                  BEGIN \
                    EXECUTE IMMEDIATE 'INSERT INTO t_out VALUES (7, ''dyn'', 7.0)'; \
                  END;",
        call: "p_dyn",
        args: &[],
        check: "SELECT id, v FROM t_out",
        want: &[&["7", "dyn"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_dynamic_truncate",
        why: "EXECUTE IMMEDIATE 'truncate table …'（考题日终清表）",
        routine: "CREATE OR REPLACE PROCEDURE p_trunc IS \
                  BEGIN \
                    INSERT INTO t_out VALUES (1, 'a', 1); \
                    EXECUTE IMMEDIATE 'truncate table t_out'; \
                    INSERT INTO t_out VALUES (2, 'after', 2); \
                  END;",
        call: "p_trunc",
        args: &[],
        check: "SELECT id, v FROM t_out",
        want: &[&["2", "after"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_open_for",
        why: "OPEN … FOR 动态结果集（语料 1 处，风险金段）",
        routine: "CREATE OR REPLACE PROCEDURE p_opensif(o OUT VARCHAR2) IS \
                  TYPE t IS REF CURSOR; \
                  c t; v_sql VARCHAR2(200); v_f VARCHAR2(20); \
                  BEGIN \
                    v_sql := 'SELECT fund FROM t_item WHERE kind = ''BOND'''; \
                    OPEN c FOR v_sql; \
                    FETCH c INTO v_f; \
                    CLOSE c; \
                    o := v_f; \
                  END;",
        call: "p_opensif",
        args: &["null"],
        check: "",
        want: &[],
        out_param: "o",
        want_out: "F01",
    },
    R {
        key: "pl_isopen",
        why: "%ISOPEN 游标状态判定（语料 6 处）",
        routine: "CREATE OR REPLACE PROCEDURE p_isopen(o OUT VARCHAR2) IS \
                  CURSOR c IS SELECT fund FROM t_item; \
                  v VARCHAR2(20); \
                  BEGIN \
                    IF c%ISOPEN THEN o := 'leak'; ELSE o := 'closed'; END IF; \
                    OPEN c; \
                    FETCH c INTO v; \
                    CLOSE c; \
                  END;",
        call: "p_isopen",
        args: &["null"],
        check: "",
        want: &[],
        out_param: "o",
        want_out: "closed",
    },
    R {
        key: "pl_param_mode",
        why: "IN OUT 参双向传递（语料 66 处）",
        routine: "CREATE OR REPLACE PROCEDURE p_io(x IN OUT NUMBER) IS \
                  BEGIN x := x + 1; INSERT INTO t_out VALUES (x, 'io', x); END;",
        call: "p_io",
        args: &["41"],
        check: "SELECT id, v FROM t_out",
        want: &[&["42", "io"]],
        out_param: "x",
        want_out: "42",
    },
    R {
        key: "pl_package",
        why: "包声明 + 包体 + 包过程写表（语料 20 处）",
        routine: "CREATE OR REPLACE PACKAGE pkg_demo IS \
                  FUNCTION f_fund(p IN NUMBER) RETURN VARCHAR2; \
                  PROCEDURE p_touch(p IN NUMBER); \
                END;\n\
                  CREATE OR REPLACE PACKAGE BODY pkg_demo IS \
                  FUNCTION f_fund(p IN NUMBER) RETURN VARCHAR2 IS v VARCHAR2(20); \
                  BEGIN SELECT fund INTO v FROM t_item WHERE id = p; RETURN v; END; \
                  PROCEDURE p_touch(p IN NUMBER) IS \
                  BEGIN INSERT INTO t_out VALUES (p, pkg_demo.f_fund(p), 0); END; \
                END;",
        call: "pkg_demo.p_touch",
        args: &["3"],
        check: "SELECT id, v FROM t_out",
        want: &[&["3", "F01"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_package_body",
        why: "包体内互相调用（语料 10 处）",
        routine: "CREATE OR REPLACE PACKAGE pkg_two IS \
                  FUNCTION f_a(p IN NUMBER) RETURN VARCHAR2; \
                  FUNCTION f_b(p IN NUMBER) RETURN VARCHAR2; \
                END;\n\
                  CREATE OR REPLACE PACKAGE BODY pkg_two IS \
                  FUNCTION f_a(p IN NUMBER) RETURN VARCHAR2 IS v VARCHAR2(20); \
                  BEGIN SELECT fund INTO v FROM t_item WHERE id = p; RETURN v; END; \
                  FUNCTION f_b(p IN NUMBER) RETURN VARCHAR2 IS \
                  BEGIN RETURN 'b:' || pkg_two.f_a(p); END; \
                END;",
        call: "pkg_two.f_b",
        args: &["4"],
        check: "",
        want: &[],
        out_param: "",
        want_out: "b:F02",
    },
    R {
        key: "pl_package_call",
        why: "SQL 上下文调用包例程 → 参数元组下沉",
        routine: "CREATE OR REPLACE PACKAGE pkg_calc IS \
                  FUNCTION f_tax(amt IN DECIMAL(18,2)) RETURN DECIMAL(18,2); \
                END;\n\
                  CREATE OR REPLACE PACKAGE BODY pkg_calc IS \
                  FUNCTION f_tax(amt IN DECIMAL(18,2)) RETURN DECIMAL(18,2) IS \
                  BEGIN RETURN ROUND(amt * 0.001, 2); END; \
                END;\n\
                  CREATE OR REPLACE PROCEDURE p_calc IS v DECIMAL(18,2); \
                  BEGIN \
                    SELECT pkg_calc.f_tax(amt) INTO v FROM t_item WHERE id = 3; \
                    INSERT INTO t_out VALUES (3, 'tax', v); \
                  END;",
        call: "p_calc",
        args: &[],
        check: "SELECT id, v, n FROM t_out",
        want: &[&["3", "tax", "5"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_goto",
        why: "GOTO 到错误处理段（语料主流错误行形态）",
        routine: "CREATE OR REPLACE PROCEDURE p_goto IS \
                  BEGIN \
                    INSERT INTO t_out VALUES (1, 'pre', 1); \
                    GOTO err_seg; \
                    INSERT INTO t_out VALUES (2, 'never', 2); \
                    <<err_seg>> \
                    INSERT INTO t_out VALUES (3, 'err', 3); \
                  END;",
        call: "p_goto",
        args: &[],
        check: "SELECT id, v FROM t_out ORDER BY id",
        want: &[&["1", "pre"], &["3", "err"]],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_label",
        why: "错误行跳转：处理器内 GOTO 到循环续点 <<next_row>>，跳过本行写入\
              （语料 13 处 <<label>> / 24 处 GOTO error_row 的主流形态）",
        routine: "CREATE OR REPLACE PROCEDURE p_label IS \
                  CURSOR c IS SELECT id, fund FROM t_item ORDER BY id; \
                  v_id NUMBER; v_f VARCHAR2(20); v_fee DECIMAL(18,2); n NUMBER := 0; \
                  BEGIN \
                    OPEN c; \
                    LOOP \
                      FETCH c INTO v_id, v_f; \
                      EXIT WHEN c%NOTFOUND; \
                      BEGIN \
                        SELECT fee INTO v_fee FROM t_item WHERE id = v_id AND kind = 'STOCK'; \
                      EXCEPTION \
                        WHEN NO_DATA_FOUND THEN GOTO next_row; \
                      END; \
                      INSERT INTO t_out VALUES (v_id, v_f, v_fee); \
                      n := n + 1; \
                      <<next_row>> \
                      NULL; \
                    END LOOP; \
                    CLOSE c; \
                    INSERT INTO t_out VALUES (999, 'rows', n); \
                  END;",
        call: "p_label",
        args: &[],
        check: "SELECT id, v FROM t_out ORDER BY id",
        want: &[
            &["1", "F01"],
            &["2", "F01"],
            &["4", "F02"],
            &["999", "rows"],
        ],
        out_param: "",
        want_out: "",
    },
    R {
        key: "pl_exception_when_others",
        why: "WHEN OTHERS 记日志后继续（语料 156 处，吞错形态）",
        routine: "CREATE OR REPLACE PROCEDURE p_others(o OUT VARCHAR2) IS \
                  v NUMBER; \
                  BEGIN \
                    v := 1 / 0; \
                    o := 'no'; \
                  EXCEPTION WHEN OTHERS THEN \
                    INSERT INTO t_out VALUES (1, 'logged', 0); \
                    o := 'caught'; \
                  END;",
        call: "p_others",
        args: &["null"],
        check: "SELECT COUNT(*) FROM t_out",
        want: &[&["1"]],
        out_param: "o",
        want_out: "caught",
    },
    R {
        key: "pl_sqlerrm_sqlcode",
        why: "SQLERRM 写入错误表（语料 32 处）。取前 8 字符锁定内核错误前缀：\
              文本由内核给出，随内核版本复核（AGENTS §1 内核锚定 + validation V6/V21）",
        routine: "CREATE OR REPLACE PROCEDURE p_err(o OUT VARCHAR2) IS \
                  v NUMBER; \
                  BEGIN \
                    v := TO_NUMBER('abc'); \
                    o := 'no'; \
                  EXCEPTION WHEN OTHERS THEN \
                    o := SUBSTR(SQLERRM, 1, 8); \
                  END;",
        call: "p_err",
        args: &["null"],
        check: "",
        want: &[],
        out_param: "o",
        want_out: "Conversi",
    },
    R {
        key: "pl_named_exception",
        why: "具名处理器 WHEN TOO_MANY_ROWS 记错误行（语料 5 处：SELECT INTO 命中多行）。\
              语料零命中 `RAISE <具名>`，该形态编译期显式拒绝（BIZ-0202）",
        routine: "CREATE OR REPLACE PROCEDURE p_named(p IN VARCHAR2, o OUT VARCHAR2) IS \
                  v VARCHAR2(20); \
                  BEGIN \
                    SELECT fund INTO v FROM t_item WHERE kind = p; \
                    o := 'one'; \
                  EXCEPTION \
                    WHEN TOO_MANY_ROWS THEN \
                      INSERT INTO t_out VALUES (1, 'many', 0); \
                      o := 'many'; \
                  END;",
        call: "p_named",
        args: &["'STOCK'", "null"],
        check: "SELECT id, v FROM t_out",
        want: &[&["1", "many"]],
        out_param: "o",
        want_out: "many",
    },
    R {
        key: "pl_exception_decl",
        why: "声明段异常常量 `ex EXCEPTION;`（语料 5 处 e1/ex：只声明，抛出靠内建错误触发、\
              捕获靠 OTHERS 兜底）",
        routine: "CREATE OR REPLACE PROCEDURE p_decl(o OUT VARCHAR2) IS \
                  ex EXCEPTION; \
                  v DECIMAL(18,2); \
                  BEGIN \
                    SELECT amt INTO v FROM t_item WHERE id = 3; \
                    o := v; \
                  EXCEPTION WHEN OTHERS THEN o := 'caught'; \
                  END;",
        call: "p_decl",
        args: &["null"],
        check: "",
        want: &[],
        out_param: "o",
        want_out: "5000",
    },
];
