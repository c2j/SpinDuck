//! PL 函数形态：声明、赋值、控制流、异常与 `%TYPE`（语料函数面）。

#![allow(dead_code)]

pub struct F {
    pub key: &'static str,
    pub why: &'static str,
    pub routine: &'static str,
    pub call: &'static str,
    pub args: &'static [&'static str],
    pub want: &'static str,
}

pub const FNS: &[F] = &[
    F {
        key: "pl_function",
        why: "函数声明 + RETURN（语料 32 处）",
        routine: "CREATE OR REPLACE FUNCTION f_ret(p IN VARCHAR2) RETURN VARCHAR2 IS \
                  BEGIN RETURN p; END;",
        call: "f_ret",
        args: &["'ok'"],
        want: "ok",
    },
    F {
        key: "pl_number_type",
        why: "NUMBER 精度声明的入参与返回",
        routine: "CREATE OR REPLACE FUNCTION f_num(p IN NUMBER(8,2)) RETURN NUMBER(8,2) IS \
                  BEGIN RETURN p; END;",
        call: "f_num",
        args: &["12.5"],
        want: "12.5",
    },
    F {
        key: "pl_assign",
        why: "变量赋值 :=（语料 550 处）",
        routine: "CREATE OR REPLACE FUNCTION f_assign(p IN NUMBER) RETURN VARCHAR2 IS \
                  v VARCHAR2(10); \
                  BEGIN v := 'init'; IF p > 0 THEN v := 'pos'; END IF; RETURN v; END;",
        call: "f_assign",
        args: &["1"],
        want: "pos",
    },
    F {
        key: "pl_varchar2",
        why: "VARCHAR2 变量声明与拼接返回",
        routine: "CREATE OR REPLACE FUNCTION f_vc2(p IN VARCHAR2) RETURN VARCHAR2 IS \
                  v VARCHAR2(50); \
                  BEGIN v := 'A' || p; RETURN v; END;",
        call: "f_vc2",
        args: &["'B'"],
        want: "AB",
    },
    F {
        key: "pl_if",
        why: "IF/ELSIF/ELSE 链（语料 117 处）",
        routine: "CREATE OR REPLACE FUNCTION f_if(p IN NUMBER) RETURN VARCHAR2 IS \
                  BEGIN \
                    IF p > 10 THEN RETURN 'big'; \
                    ELSIF p > 1 THEN RETURN 'mid'; \
                    ELSE RETURN 'small'; \
                    END IF; \
                  END;",
        call: "f_if",
        args: &["5"],
        want: "mid",
    },
    F {
        key: "pl_elsif",
        why: "ELSIF 分支取第三档",
        routine: "CREATE OR REPLACE FUNCTION f_if3(p IN NUMBER) RETURN VARCHAR2 IS \
                  BEGIN \
                    IF p > 10 THEN RETURN 'big'; \
                    ELSIF p > 1 THEN RETURN 'mid'; \
                    ELSIF p > 0 THEN RETURN 'one'; \
                    ELSE RETURN 'small'; \
                    END IF; \
                  END;",
        call: "f_if3",
        args: &["1"],
        want: "one",
    },
    F {
        key: "pl_case",
        why: "PL CASE 语句分派（语料 6 处）",
        routine: "CREATE OR REPLACE FUNCTION f_case(p IN VARCHAR2) RETURN VARCHAR2 IS \
                  v VARCHAR2(10); \
                  BEGIN \
                    CASE p \
                      WHEN '1' THEN v := 'one'; \
                      WHEN '2' THEN v := 'two'; \
                      ELSE v := 'other'; \
                    END CASE; \
                    RETURN v; \
                  END;",
        call: "f_case",
        args: &["'2'"],
        want: "two",
    },
    F {
        key: "pl_loop",
        why: "LOOP + EXIT WHEN 累加（语料 17 处）",
        routine: "CREATE OR REPLACE FUNCTION f_loop(p IN NUMBER) RETURN NUMBER IS \
                  i NUMBER := 0; s NUMBER := 0; \
                  BEGIN \
                    LOOP \
                      i := i + 1; \
                      s := s + i; \
                      EXIT WHEN i >= p; \
                    END LOOP; \
                    RETURN s; \
                  END;",
        call: "f_loop",
        args: &["4"],
        want: "10",
    },
    F {
        key: "pl_for_loop",
        why: "FOR i IN 区间循环（语料 2 处）",
        routine: "CREATE OR REPLACE FUNCTION f_forloop(p IN NUMBER) RETURN NUMBER IS \
                  s NUMBER := 0; \
                  BEGIN \
                    FOR i IN 1..p LOOP s := s + i; END LOOP; \
                    RETURN s; \
                  END;",
        call: "f_forloop",
        args: &["5"],
        want: "15",
    },
    F {
        key: "pl_goto",
        why: "GOTO 跳出多层循环（语料 33 处，错误行跳转形态）",
        routine: "CREATE OR REPLACE FUNCTION f_goto(p IN NUMBER) RETURN VARCHAR2 IS \
                  v VARCHAR2(10); \
                  BEGIN \
                    FOR i IN 1..5 LOOP \
                      IF i = p THEN GOTO skip; END IF; \
                    END LOOP; \
                    v := 'full'; \
                    <<skip>> \
                    RETURN 'jumped'; \
                  END;",
        call: "f_goto",
        args: &["2"],
        want: "jumped",
    },
    F {
        key: "pl_label",
        why: "<<label>> 作为正常落点（语料 13 处）",
        routine: "CREATE OR REPLACE FUNCTION f_label RETURN VARCHAR2 IS \
                  BEGIN \
                    GOTO end_here; \
                    RETURN 'skipped'; \
                    <<end_here>> \
                    RETURN 'ended'; \
                  END;",
        call: "f_label",
        args: &[],
        want: "ended",
    },
    F {
        key: "pl_exception_when_others",
        why: "WHEN OTHERS 兜底（语料 156 处）",
        routine: "CREATE OR REPLACE FUNCTION f_others RETURN VARCHAR2 IS \
                  v NUMBER; \
                  BEGIN \
                    v := 1 / 0; \
                    RETURN 'no'; \
                  EXCEPTION WHEN OTHERS THEN RETURN 'caught'; END;",
        call: "f_others",
        args: &[],
        want: "caught",
    },
    F {
        key: "pl_named_exception",
        why: "具名处理器 WHEN TOO_MANY_ROWS（语料 5 处：SELECT INTO 命中多行）。零命中形态 \
              `RAISE <具名>` 编译期显式拒绝（BIZ-0202），不再虚构消息",
        routine: "CREATE OR REPLACE FUNCTION f_tm(p IN VARCHAR2) RETURN VARCHAR2 IS \
                  v VARCHAR2(20); \
                  BEGIN \
                    SELECT fund INTO v FROM t_item WHERE kind = p; \
                    RETURN 'one'; \
                  EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN 'many'; END;",
        call: "f_tm",
        args: &["'STOCK'"],
        want: "many",
    },
    F {
        key: "pl_raise",
        why: "处理器内裸 `RAISE;` 原样上抛（语料 10 处；零命中 RAISE_APPLICATION_ERROR）。\
              重抛保留错误码/故障域，调用方仍见原始错误",
        routine: "CREATE OR REPLACE FUNCTION f_rs RETURN VARCHAR2 IS \
                  v NUMBER; \
                  BEGIN \
                    v := 1 / 0; \
                    RETURN 'no'; \
                  EXCEPTION WHEN OTHERS THEN \
                    INSERT INTO t_out VALUES (1, 'log', 0); \
                    RAISE; \
                  END;",
        call: "f_rs",
        args: &[],
        want: "ERR:division by zero",
    },
    F {
        key: "pl_sqlerrm_sqlcode",
        why: "SQLERRM 取错误上下文（语料 32 处）。前 4 字符锁定内核错误前缀，\
              文本随内核版本复核（§1 内核锚定 + validation V6/V21）；SQLCODE 语料零命中",
        routine: "CREATE OR REPLACE FUNCTION f_err RETURN VARCHAR2 IS \
                  v NUMBER; \
                  BEGIN \
                    v := TO_NUMBER('abc'); \
                    RETURN 'none'; \
                  EXCEPTION WHEN OTHERS THEN RETURN SUBSTR(SQLERRM, 1, 4); END;",
        call: "f_err",
        args: &[],
        want: "Conv",
    },
    F {
        key: "pl_type_attr",
        why: "%TYPE 变量类型继承（语料 272 处）",
        routine: "CREATE OR REPLACE FUNCTION f_pct(p IN VARCHAR2) RETURN VARCHAR2 IS \
                  v t_form.s%TYPE; \
                  BEGIN v := p; RETURN v; END;",
        call: "f_pct",
        args: &["'PCT'"],
        want: "PCT",
    },
    F {
        key: "pl_param_default",
        why: "形参 DEFAULT 值（语料 22 处：`VARCHAR2 DEFAULT NULL`、`VARCHAR2 DEFAULT '0'`、\
              `NUMBER DEFAULT 0`；裸 DEFAULT 计数含 DDL 列默认值，不作本形态口径）",
        routine: "CREATE OR REPLACE FUNCTION f_def(p IN NUMBER DEFAULT 0, q IN VARCHAR2 DEFAULT '0') \
                  RETURN VARCHAR2 IS BEGIN RETURN p || '/' || q; END;",
        call: "f_def",
        args: &[],
        want: "0/0",
    },
    F {
        key: "pl_date_switch_assign",
        why: "TO_DATE 装载 + 日期分支赋值（语料 75 处，日终切换形态）。语料用 TO_DATE 比较，\
              不用 `DATE '…'` 字面量（该形态零命中）",
        routine: "CREATE OR REPLACE FUNCTION f_dsw(p IN VARCHAR2) RETURN VARCHAR2 IS \
                  d DATE; \
                  BEGIN \
                    d := TO_DATE(p, 'YYYYMMDD'); \
                    IF d >= TO_DATE('20250701', 'YYYYMMDD') THEN RETURN 'H2'; ELSE RETURN 'H1'; END IF; \
                  END;",
        call: "f_dsw",
        args: &["'20250715'"],
        want: "H2",
    },
    F {
        key: "select_into",
        why: "SELECT … INTO 单行取值（语料 155 处）",
        routine: "CREATE OR REPLACE FUNCTION f_into(p IN NUMBER) RETURN VARCHAR2 IS \
                  v VARCHAR2(20); \
                  BEGIN SELECT s INTO v FROM t_form WHERE id = p; RETURN v; END;",
        call: "f_into",
        args: &["1"],
        want: "AbC",
    },
    F {
        key: "pl_notfound_found",
        why: "%NOTFOUND 之外：NO_DATA_FOUND 兜底（语料 17 处）",
        routine: "CREATE OR REPLACE FUNCTION f_ndf RETURN VARCHAR2 IS \
                  v VARCHAR2(20); \
                  BEGIN \
                    SELECT s INTO v FROM t_form WHERE id = 999; \
                    RETURN v; \
                  EXCEPTION WHEN NO_DATA_FOUND THEN RETURN 'none'; END;",
        call: "f_ndf",
        args: &[],
        want: "none",
    },
    F {
        key: "call_within_call",
        why: "SQL 上下文中调用另一例程 → 参数元组下沉（语料 1209 处嵌套调用）",
        routine: "CREATE OR REPLACE FUNCTION f_inner(k IN VARCHAR2) RETURN VARCHAR2 IS \
                  BEGIN RETURN NVL(k, 'nil'); END;\n\
                  CREATE OR REPLACE FUNCTION f_outer(p IN NUMBER) RETURN VARCHAR2 IS \
                  v VARCHAR2(20); \
                  BEGIN SELECT f_inner(s) INTO v FROM t_form WHERE id = p; RETURN v; END;",
        call: "f_outer",
        args: &["3"],
        want: "nil",
    },
    F {
        key: "pl_isopen",
        why: "%ISOPEN 游标状态判定（语料 6 处）",
        routine: "CREATE OR REPLACE FUNCTION f_isopen RETURN VARCHAR2 IS \
                  CURSOR c IS SELECT s FROM t_form; \
                  v VARCHAR2(20); \
                  BEGIN \
                    IF c%ISOPEN THEN RETURN 'open'; END IF; \
                    OPEN c; \
                    FETCH c INTO v; \
                    CLOSE c; \
                    RETURN v; \
                  END;",
        call: "f_isopen",
        args: &[],
        want: "AbC",
    },
];
