//! Gate 2B prepare 编排单元测试（自 `prepare.rs` 拆分，AGENTS §3）。

use super::*;
use crate::ir::VarName;
use ogsql_parser::ast::Statement;
use ogsql_parser::parser::Parser;
use ogsql_parser::token::tokenizer::Tokenizer;

fn snap_of(sql: &str) -> SchemaSnapshot {
    let toks = Tokenizer::new(sql).tokenize().unwrap();
    let stmts: Vec<Statement> = Parser::new(toks).parse();
    SchemaSnapshot::from_statements(&stmts).unwrap()
}

#[test]
fn should_emit_stub_ddl_for_known_objects_only() {
    let snap = snap_of(
        "CREATE TABLE s.t1(a INT, b VARCHAR2(10)); \
             CREATE VIEW v1(x, y) AS SELECT a, b FROM s.t1; \
             CREATE VIEW v2 AS SELECT a FROM s.t1; \
             CREATE SEQUENCE seq1; \
             CREATE TABLE tmp AS SELECT a FROM s.t1;",
    );
    let rules = RuleRegistry::default();
    let ddl = stub_ddl(&snap, &rules);
    assert!(ddl.iter().any(|d| d == "CREATE SCHEMA IF NOT EXISTS \"s\""));
    assert!(ddl.iter().any(|d| d.contains("CREATE TABLE \"s\".\"t1\"")));
    assert!(ddl
        .iter()
        .any(|d| d.contains("CREATE VIEW \"v1\" (\"x\", \"y\")")));
    assert!(ddl.iter().any(|d| d.contains("CREATE SEQUENCE \"seq1\"")));
    // CTAS 延迟表与未知视图不建模
    assert!(!ddl.iter().any(|d| d.contains("\"tmp\"")));
    assert!(!ddl.iter().any(|d| d.contains("\"v2\"")));
}

#[test]
fn should_emit_schema_for_qualified_view_and_sequence_only() {
    // 仅有 s.v / s.seq、无 s 下表时也必须先 CREATE SCHEMA
    let snap = snap_of("CREATE VIEW s.v1(x) AS SELECT 1; CREATE SEQUENCE s.seq1;");
    let ddl = stub_ddl(&snap, &RuleRegistry::default());
    assert_eq!(ddl[0], "CREATE SCHEMA IF NOT EXISTS \"s\"", "{ddl:?}");
    assert!(
        ddl.iter().any(|d| d.contains("CREATE VIEW \"s\".\"v1\"")),
        "{ddl:?}"
    );
    assert!(
        ddl.iter()
            .any(|d| d.contains("CREATE SEQUENCE \"s\".\"seq1\"")),
        "{ddl:?}"
    );
}

#[test]
fn should_map_custom_type_via_rules() {
    let snap = snap_of("CREATE TABLE t1(a NUMBER(10,2), b VARCHAR2(20))");
    let mut rules = RuleRegistry::default();
    rules.types.insert("NUMBER".into(), "DECIMAL".into());
    rules.types.insert("VARCHAR2".into(), "VARCHAR".into());
    let ddl = stub_ddl(&snap, &rules);
    assert_eq!(ddl.len(), 1);
    assert!(ddl[0].contains("\"a\" DECIMAL"), "{ddl:?}");
    assert!(ddl[0].contains("\"b\" VARCHAR"), "{ddl:?}");
}

#[test]
fn should_include_deferred_and_unknown_names() {
    let snap = snap_of(
        "CREATE TABLE t1(a INT); CREATE TABLE tmp AS SELECT a FROM t1; \
             CREATE VIEW vu AS SELECT a FROM t1;",
    );
    let names = deferred_names(&snap);
    assert!(names.contains("tmp"));
    assert!(names.contains("vu"));
    assert!(names.contains("sys_dummy"));
    assert!(names.contains("dual"));
    assert!(!names.contains("t1"));
}

#[test]
fn should_only_suppress_object_missing_errors() {
    let names: BTreeSet<String> = ["tmp".to_string(), "missing_t".to_string()].into();
    // 对象不存在且点名延迟对象 → 抑制
    assert!(mentions(
        "Catalog Error: Table with name tmp does not exist!",
        &names
    ));
    // 列级错误即使回显里出现延迟表名 → 不抑制
    assert!(!mentions(
        "Binder Error: Referenced column \"tmp\" not found in FROM clause!",
        &names
    ));
    // 候选列回显含同名 → 不抑制
    assert!(!mentions("Candidate bindings: \"t1\".\"tmp\"", &names));
    // 不点名任何延迟对象 → 不抑制
    assert!(!mentions(
        "Catalog Error: Table with name other_t does not exist!",
        &names
    ));
}

#[test]
fn should_not_suppress_near_miss_object_names() {
    // 词边界：延迟名 dual/tmp 不得命中 dual_acct/tmp_hist/s.tmp_hist
    let names: BTreeSet<String> =
        ["dual".to_string(), "tmp".to_string(), "s.tmp".to_string()].into();
    for err in [
        "Catalog Error: Table with name ods.dual_acct does not exist!",
        "Catalog Error: Table with name s.tmp_hist does not exist!",
        "Catalog Error: Table with name tmp2 does not exist!",
    ] {
        assert!(!mentions(err, &names), "{err}");
    }
    // 精确命中（含限定名与引号形态）→ 抑制
    for err in [
        "Catalog Error: Table with name tmp does not exist!",
        "Catalog Error: Table with name \"s\".\"tmp\" does not exist!",
        "Catalog Error: Table with name s.tmp does not exist!",
    ] {
        assert!(mentions(err, &names), "{err}");
    }
}

#[test]
fn should_type_placeholder_vars() {
    let stmt = PrepareStmt {
        file: "f".into(),
        sql: "SELECT * FROM t WHERE d = $v_d AND n = $v_n".into(),
        binds: vec![
            ("$v_d".into(), BindSrc::Var(VarName("d".into()))),
            ("$v_n".into(), BindSrc::Var(VarName("n".into()))),
        ],
        var_types: BTreeMap::from([
            ("d".to_string(), SpinType::Date),
            ("n".to_string(), SpinType::Int),
        ]),
        skip: BTreeSet::new(),
        ddl_exec: None,
    };
    let sql = typed_sql(&stmt);
    assert!(sql.contains("CAST(NULL AS DATE)"), "{sql}");
    assert!(sql.contains("CAST(NULL AS INTEGER)"), "{sql}");
    assert!(!sql.contains("$v_d"), "{sql}");
}

#[test]
fn should_not_corrupt_prefix_placeholders_or_literals() {
    // $v_v_return_rate 是 $v_v_return_rate_flag 的前缀：最长优先 +
    // 右标识符边界，两个占位符必须各自完整替换；字符串里的不动。
    let stmt = PrepareStmt {
        file: "f".into(),
        sql: "SELECT $v_v_return_rate, $v_v_return_rate_flag, '$v_v_return_rate' FROM t".into(),
        binds: vec![
            (
                "$v_v_return_rate".into(),
                BindSrc::Var(VarName("v_return_rate".into())),
            ),
            (
                "$v_v_return_rate_flag".into(),
                BindSrc::Var(VarName("v_return_rate_flag".into())),
            ),
        ],
        var_types: BTreeMap::from([
            (
                "v_return_rate".to_string(),
                SpinType::Decimal {
                    precision: 10,
                    scale: 2,
                },
            ),
            ("v_return_rate_flag".to_string(), SpinType::Bool),
        ]),
        skip: BTreeSet::new(),
        ddl_exec: None,
    };
    let sql = typed_sql(&stmt);
    assert_eq!(
        sql, "SELECT CAST(NULL AS DECIMAL), CAST(NULL AS BOOLEAN), '$v_v_return_rate' FROM t",
        "{sql}"
    );
}

#[cfg(feature = "native")]
#[test]
fn should_prepare_clean_sql_and_suppress_deferred_errors() {
    let snap = snap_of("CREATE TABLE t1(a INT, b VARCHAR); CREATE TABLE tmp AS SELECT a FROM t1;");
    let rules = RuleRegistry::default();
    let mk = |sql: &str, skip: &[&str]| PrepareStmt {
        file: "f1".to_string(),
        sql: sql.to_string(),
        binds: Vec::new(),
        var_types: BTreeMap::new(),
        skip: skip.iter().map(|s| s.to_string()).collect(),
        ddl_exec: None,
    };
    let sqls = vec![
        mk("SELECT a FROM t1", &[]),
        mk("SELECT nope FROM t1", &[]),
        mk("SELECT a FROM tmp", &[]),
        mk("SELECT a FROM __sd_fn_tmp", &["__sd_fn_tmp"]),
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    // 仅真实缺列报 KERNEL-0011；延迟表与下沉临时表的"不存在"错误被抑制
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, "f1");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
#[test]
fn should_prepare_schema_qualified_stub_table() {
    let snap = snap_of("CREATE TABLE s.t1(a INT)");
    let rules = RuleRegistry::default();
    let sqls = vec![PrepareStmt {
        file: "f".into(),
        sql: "SELECT a FROM s.t1".into(),
        binds: Vec::new(),
        var_types: BTreeMap::new(),
        skip: BTreeSet::new(),
        ddl_exec: None,
    }];
    // schema 已建 → prepare 通过；若漏建 schema 会报 KERNEL-0011
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert!(out.is_empty(), "{out:?}");
}

#[cfg(feature = "native")]
#[test]
fn should_catch_typed_placeholder_overload_error() {
    // $v_d 类型化为 DATE 后，round(date,'MM') 这类重载错误能被 Binder 捕获；
    // 不替换时 DuckDB 把 $v_d 当参数，prepare 不报错（漏报）。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let sqls = vec![PrepareStmt {
        file: "f".into(),
        sql: "SELECT round($v_d, 'MM') FROM t1".into(),
        binds: vec![("$v_d".into(), BindSrc::Var(VarName("d".into())))],
        var_types: BTreeMap::from([("d".to_string(), SpinType::Date)]),
        skip: BTreeSet::new(),
        ddl_exec: None,
    }];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
#[test]
fn should_judge_same_sql_with_different_types_and_skip() {
    // 同文本不同 var_types：DATE 的 round 重载错误必须上报，
    // 不能被先到的"通过"语句去重吞掉；skip 不同同理。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let mk = |ty: SpinType, skip: &[&str]| PrepareStmt {
        file: "f".into(),
        sql: "SELECT round($v_d, 'MM') FROM t1".into(),
        binds: vec![("$v_d".into(), BindSrc::Var(VarName("d".into())))],
        var_types: BTreeMap::from([("d".to_string(), ty)]),
        skip: skip.iter().map(|s| s.to_string()).collect(),
        ddl_exec: None,
    };
    let sqls = vec![mk(SpinType::Int, &[]), mk(SpinType::Date, &[])];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    // Int 版 prepare 通过；Date 版 KERNEL-0011——去重不得跨类型合并
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
fn ddl_stmt(sql: &str, kind: DdlExec) -> PrepareStmt {
    PrepareStmt {
        file: "f".into(),
        sql: sql.to_string(),
        binds: Vec::new(),
        var_types: BTreeMap::new(),
        skip: BTreeSet::new(),
        ddl_exec: Some(kind),
    }
}

#[cfg(feature = "native")]
fn ddl_create(sql: &str, name: &str, ctas: bool) -> PrepareStmt {
    ddl_stmt(
        sql,
        if ctas {
            DdlExec::CreateTableAs(LocalDdl {
                name: name.to_string(),
                temporary: true,
                if_not_exists: false,
            })
        } else {
            DdlExec::CreateTable(LocalDdl {
                name: name.to_string(),
                temporary: true,
                if_not_exists: false,
            })
        },
    )
}

#[cfg(feature = "native")]
fn ddl_drop(name: &str) -> PrepareStmt {
    ddl_stmt(
        &format!("DROP TABLE IF EXISTS {name}"),
        DdlExec::DropLocal(name.to_string()),
    )
}

#[cfg(feature = "native")]
#[test]
fn should_execute_local_ddl_and_check_real_columns() {
    // 例程体内 CREATE TEMP TABLE 在 stub 库真实建表：后续语句的
    // 列检查以它为准（缺列 → KERNEL-0011，不再一律延迟豁免）。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let sqls = vec![
        ddl_create("CREATE TEMP TABLE tmp(a INTEGER)", "\"tmp\"", false),
        PrepareStmt {
            file: "f".into(),
            sql: "SELECT b FROM tmp".into(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: BTreeSet::new(),
            ddl_exec: None,
        },
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
#[test]
fn should_execute_ctas_local_ddl_with_limit0() {
    // CTAS 追加 LIMIT 0 执行：只为 binder 发现列，不物化数据。
    let snap = snap_of("CREATE TABLE t1(a INT, b VARCHAR)");
    let rules = RuleRegistry::default();
    let sqls = vec![
        ddl_create(
            "CREATE TEMP TABLE t2 AS SELECT a, b FROM t1",
            "\"t2\"",
            true,
        ),
        PrepareStmt {
            file: "f".into(),
            sql: "SELECT a FROM t2".into(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: BTreeSet::new(),
            ddl_exec: None,
        },
        PrepareStmt {
            file: "f".into(),
            sql: "SELECT z FROM t2".into(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: BTreeSet::new(),
            ddl_exec: None,
        },
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
#[test]
fn should_isolate_local_tables_between_routines() {
    // 例程 A 的 tmp(a) 收尾 DROP 后，例程 B 的 tmp(b) 不得冲突；
    // 同文本 `SELECT b FROM tmp` 在两个 catalog epoch 下必须分别
    // 裁决——epoch 去重防止 B 版被 A 版吞掉。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let q = |sql: &str| PrepareStmt {
        file: "f".into(),
        sql: sql.to_string(),
        binds: Vec::new(),
        var_types: BTreeMap::new(),
        skip: BTreeSet::new(),
        ddl_exec: None,
    };
    let sqls = vec![
        ddl_create("CREATE TEMP TABLE tmp(a INTEGER)", "\"tmp\"", false),
        q("SELECT b FROM tmp"), // A 版：缺列 → KERNEL-0011
        ddl_drop("\"tmp\""),
        ddl_create("CREATE TEMP TABLE tmp(b INTEGER)", "\"tmp\"", false),
        q("SELECT b FROM tmp"), // B 版：列存在 → 通过
        ddl_drop("\"tmp\""),
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
#[test]
fn should_build_ctas_table_when_query_has_limit() {
    // 查询自带 LIMIT：不得在句尾盲拼 LIMIT 0（DuckDB 语法错误），
    // 应把查询包成子查询只追加一次 LIMIT 0，照常建表查列。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let sqls = vec![
        ddl_create(
            "CREATE TEMP TABLE t2 AS SELECT a FROM t1 LIMIT 5",
            "\"t2\"",
            true,
        ),
        PrepareStmt {
            file: "f".into(),
            sql: "SELECT a FROM t2".into(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: BTreeSet::new(),
            ddl_exec: None,
        },
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert!(out.is_empty(), "{out:?}");
}

#[cfg(feature = "native")]
#[test]
fn should_build_ctas_table_with_no_data_clause() {
    // WITH NO DATA 是 Oracle CTAS 子句，DuckDB 不认：剥掉后照常
    // 建表（空表结构即可供 binder 查列）。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let sqls = vec![
        ddl_create(
            "CREATE TEMP TABLE t3 AS SELECT a FROM t1 WITH NO DATA",
            "\"t3\"",
            true,
        ),
        PrepareStmt {
            file: "f".into(),
            sql: "SELECT a FROM t3".into(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: BTreeSet::new(),
            ddl_exec: None,
        },
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert!(out.is_empty(), "{out:?}");
}

#[cfg(feature = "native")]
#[test]
fn should_bind_residual_placeholders_in_local_ctas() {
    // %TYPE/记录字段/未映射类型留下的 $v_*：execute 要求全部参数
    // 有绑定，残留占位符绑 NULL，不得报"未提供参数"打成 KERNEL-0011。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let mut ctas = ddl_create(
        "CREATE TEMP TABLE t2 AS SELECT a FROM t1 WHERE a = $v_id",
        "\"t2\"",
        true,
    );
    ctas.binds = vec![("$v_id".into(), BindSrc::Var(VarName("id".into())))];
    let sqls = vec![
        ctas,
        PrepareStmt {
            file: "f".into(),
            sql: "SELECT a FROM t2".into(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: BTreeSet::new(),
            ddl_exec: None,
        },
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert!(out.is_empty(), "{out:?}");
}

#[cfg(feature = "native")]
#[test]
fn should_not_drop_global_table_on_if_not_exists_noop() {
    // 非临时 `CREATE TABLE IF NOT EXISTS` 命中已有全局 stub 表 =
    // 空操作：不得记入 created，收尾 DROP 不得删全局表——后续
    // 例程对 tmp(g) 的引用必须仍解析得到。
    let snap = snap_of("CREATE TABLE tmp(g INT)");
    let rules = RuleRegistry::default();
    let q = |sql: &str| PrepareStmt {
        file: "f".into(),
        sql: sql.to_string(),
        binds: Vec::new(),
        var_types: BTreeMap::new(),
        skip: BTreeSet::new(),
        ddl_exec: None,
    };
    let sqls = vec![
        ddl_stmt(
            "CREATE TABLE IF NOT EXISTS tmp(l INTEGER)",
            DdlExec::CreateTable(LocalDdl {
                name: "\"tmp\"".to_string(),
                temporary: false,
                if_not_exists: true,
            }),
        ),
        ddl_drop("\"tmp\""),
        q("SELECT g FROM tmp"), // 全局列 → 通过（证明全局表未被误删）
        q("SELECT l FROM tmp"), // 局部列不存在于全局表 → KERNEL-0011
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
#[test]
fn should_drop_temp_table_not_global_on_name_collision() {
    // 全局 tmp(g) 与例程局部 TEMP tmp(l) 同名：收尾 DROP 必须删
    // temp 影子表，不能误删 stub 全局表——否则后续例程对全局
    // tmp 的引用会假报 "does not exist"。
    let snap = snap_of("CREATE TABLE tmp(g INT)");
    let rules = RuleRegistry::default();
    let q = |sql: &str| PrepareStmt {
        file: "f".into(),
        sql: sql.to_string(),
        binds: Vec::new(),
        var_types: BTreeMap::new(),
        skip: BTreeSet::new(),
        ddl_exec: None,
    };
    let sqls = vec![
        ddl_create("CREATE TEMP TABLE tmp(l INTEGER)", "\"tmp\"", false),
        ddl_drop("\"tmp\""),
        q("SELECT g FROM tmp"), // 全局列 → 通过（证明全局表未被误删）
        q("SELECT l FROM tmp"), // 局部列已随 temp 消失 → KERNEL-0011
    ];
    let out = prepare_all(&snap, &rules, &sqls).unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
}

#[cfg(feature = "native")]
#[test]
fn should_fail_stub_ddl_as_eng_0072() {
    // stub DDL 执行失败 = 引擎侧缺陷 ENG-0072，Err 上抛而非空通过
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let rules = RuleRegistry::default();
    let err =
        prepare_all_impl(&snap, &rules, &[], &["THIS IS NOT VALID SQL".to_string()]).unwrap_err();
    assert_eq!(err.code.to_string(), "ENG-0072", "{err:?}");
}
