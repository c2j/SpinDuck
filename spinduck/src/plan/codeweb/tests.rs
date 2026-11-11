use super::{closure_from_graph, parse_codeweb_json, ClosureContext, GraphClosure};
use crate::cli::NoopExecutor;
use crate::engine::Engine;
use crate::error::EngineError;
use crate::rules::RuleRegistry;
use std::collections::BTreeMap;
use std::path::Path;

fn close(
    json: &str,
    entry: &str,
    root: &str,
    ddl: &[(&str, &str)],
) -> Result<GraphClosure, EngineError> {
    let graph = parse_codeweb_json(json).expect("fixture json");
    let ddl_files: BTreeMap<String, String> = ddl
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let ctx = ClosureContext {
        project_root: Path::new(root),
        ddl_files: &ddl_files,
    };
    closure_from_graph(&graph, entry, &ctx)
}

fn doc(nodes: &str, edges: &str) -> String {
    format!(r#"{{"_meta":{{"ignored":true}},"nodes":[{nodes}],"edges":[{edges}]}}"#)
}

#[test]
fn should_record_unresolved_direct_as_routine_missing_with_line_zero_from_caller() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"prc_trd_hz_byfund","schema":"pkg_split_trade_step3_sh","file":"/proj/sql/routines/PKG.sql","line":1},
           {"id":2,"type":"unresolved","raw_expr":"fnc_get_fund_data_mode","context":"where"},
           {"id":3,"type":"unresolved","raw_expr":"DB.Schema.Foo","context":"select"}"#,
        r#"{"source":1,"target":2,"type":"direct","scope":"sql","file":"/proj/sql/routines/PKG.sql","line":0},
           {"source":1,"target":3,"type":"direct","scope":"sql","file":"/proj/sql/routines/PKG.sql","line":0}"#,
    );
    let got = close(
        &json,
        "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
        "/proj",
        &[],
    )
    .unwrap();
    let bare = got
        .missing
        .iter()
        .find(|m| m.object == "routine:fnc_get_fund_data_mode")
        .expect("bare unresolved");
    assert_eq!(bare.from, "pkg_split_trade_step3_sh.prc_trd_hz_byfund");
    assert_eq!(bare.at, "sql/routines/PKG.sql:0");
    assert!(bare.at.ends_with(":0"));
    let qualified = got
        .missing
        .iter()
        .find(|m| m.object == "routine:schema.foo")
        .expect("normalize keeps the last two segments");
    assert_eq!(qualified.from, "pkg_split_trade_step3_sh.prc_trd_hz_byfund");
    assert!(got.refs.iter().all(|r| r.kind != "routine"));
}

#[test]
fn should_ignore_builtin_edges_for_missing_and_refs() {
    let json = doc(
        r#"{"id":1,"type":"function","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"builtin_function","name":"coalesce","category":"c","domain":"d","file":"","line":0},
           {"id":3,"type":"java_class","fqn":"a.B","name":"B","file":"/x.java","line":1}"#,
        r#"{"source":1,"target":2,"type":"uses_builtin_function","file":"/proj/a.sql","line":0},
           {"source":1,"target":2,"type":"direct","scope":"sql","file":"/proj/a.sql","line":0},
           {"source":1,"target":3,"type":"calls_java","file":"/proj/a.sql","line":4}"#,
    );
    let got = close(&json, "caller", "/proj", &[]).unwrap();
    assert!(got.missing.is_empty(), "{:?}", got.missing);
    assert!(got.refs.is_empty(), "{:?}", got.refs);
    assert!(got.dynamic.is_empty());
}

#[test]
fn should_emit_write_and_read_when_write_condition_meets_read_or_lock() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"table","name":"t_only_write","explicit":true},
           {"id":3,"type":"table","name":"t_both","explicit":true},
           {"id":4,"type":"view","name":"v_kind","explicit":true}"#,
        r#"{"source":1,"target":2,"type":"table_access","flow_kind":"read_write","modes":["write"],"write_kinds":[],"file":"/proj/a.sql","line":2,"column_analysis":{"ignored":true}},
           {"source":1,"target":3,"type":"table_access","modes":["read","write"],"write_kinds":[],"file":"/proj/a.sql","line":3},
           {"source":1,"target":4,"type":"table_access","modes":["share"],"write_kinds":["insert"],"file":"/proj/a.sql","line":4}"#,
    );
    let ddl = [
        ("t_only_write", "sql/ddl/w.sql"),
        ("t_both", "sql/ddl/b.sql"),
        ("v_kind", "sql/ddl/v.sql"),
    ];
    let got = close(&json, "caller", "/proj", &ddl).unwrap();
    let roles = |object: &str| -> Vec<&str> {
        got.refs
            .iter()
            .filter(|r| r.object == object)
            .map(|r| r.role.as_str())
            .collect()
    };
    assert_eq!(roles("t_only_write"), vec!["write"]);
    assert_eq!(roles("t_both"), vec!["read", "write"]);
    assert_eq!(roles("v_kind"), vec!["read", "write"]);
    assert!(got
        .refs
        .iter()
        .any(|r| r.object == "v_kind" && r.kind == "view"));
    assert!(got.refs.iter().all(|r| r.from == "caller"));
    assert!(got.missing.is_empty(), "{:?}", got.missing);
}

#[test]
fn should_emit_only_read_for_share_lock_with_empty_write_kinds() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"table","name":"t_share","columns":[{"name":"id"}]}"#,
        r#"{"source":1,"target":2,"type":"table_access","modes":["share"],"write_kinds":[],"file":"/proj/a.sql","line":8},
           {"source":1,"target":2,"type":"table_access","modes":["share"],"write_kinds":[],"file":"/proj/a.sql","line":8}"#,
    );
    let got = close(&json, "caller", "/proj", &[("t_share", "sql/ddl/s.sql")]).unwrap();
    assert_eq!(got.refs.len(), 1, "{:?}", got.refs);
    assert_eq!(got.refs[0].object, "t_share");
    assert_eq!(got.refs[0].kind, "table");
    assert_eq!(got.refs[0].role, "read");
    assert_eq!(got.refs[0].from, "caller");
    assert_eq!(got.refs[0].at, "a.sql:8");
}

#[test]
fn should_pull_base_tables_through_depends_on() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"view","name":"v_main","explicit":false},
           {"id":3,"type":"table","schema":"db","name":"t_base","explicit":false}"#,
        r#"{"source":1,"target":2,"type":"table_access","modes":["read"],"write_kinds":[],"file":"/proj/a.sql","line":5},
           {"source":2,"target":3,"type":"depends_on","file":"/proj/sql/v.sql","line":9}"#,
    );
    let ddl = [
        ("v_main", "sql/views/v.sql"),
        ("db.t_base", "sql/ddl/t.sql"),
    ];
    let got = close(&json, "caller", "/proj", &ddl).unwrap();
    assert!(got.refs.iter().any(|r| {
        r.object == "v_main" && r.kind == "view" && r.role == "read" && r.from == "caller"
    }));
    assert!(got.refs.iter().any(|r| {
        r.object == "db.t_base"
            && r.kind == "table"
            && r.role == "read"
            && r.from == "v_main"
            && r.at == "sql/v.sql:9"
    }));
    assert_eq!(
        got.ddl_tables,
        vec!["sql/ddl/t.sql".to_string(), "sql/views/v.sql".to_string()]
    );
    assert!(got.missing.is_empty(), "{:?}", got.missing);
}

#[test]
fn should_record_anchors_on_as_type_role_without_a_read_row() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"table","name":"t_ty","explicit":true}"#,
        r#"{"source":1,"target":2,"type":"anchors_on","file":"/proj/a.sql","line":6,"kind":"column","site":{"ignored":true}}"#,
    );
    let got = close(&json, "caller", "/proj", &[("t_ty", "sql/ddl/t.sql")]).unwrap();
    assert_eq!(got.refs.len(), 1, "{:?}", got.refs);
    assert_eq!(got.refs[0].object, "t_ty");
    assert_eq!(got.refs[0].kind, "table");
    assert_eq!(got.refs[0].role, "type");
    assert_eq!(got.refs[0].at, "a.sql:6");
    assert!(got.missing.is_empty());
}

#[test]
fn should_record_uses_sequence_as_sequence_read_and_keep_sequence_prefix_on_missing() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"sequence","name":"seq_miss","explicit":false},
           {"id":3,"type":"sequence","schema":"sch","name":"seq_ok","explicit":true}"#,
        r#"{"source":1,"target":2,"type":"uses_sequence","file":"/proj/a.sql","line":7},
           {"source":1,"target":3,"type":"uses_sequence","file":"/proj/a.sql","line":8}"#,
    );
    let got = close(
        &json,
        "caller",
        "/proj",
        &[("sch.seq_ok", "sql/ddl/seq.sql")],
    )
    .unwrap();
    assert!(got.refs.iter().any(|r| {
        r.object == "seq_miss" && r.kind == "sequence" && r.role == "read" && r.from == "caller"
    }));
    assert!(got
        .refs
        .iter()
        .any(|r| { r.object == "sch.seq_ok" && r.kind == "sequence" && r.role == "read" }));
    assert!(got
        .missing
        .iter()
        .any(|m| m.object == "sequence:seq_miss" && m.from == "caller"));
    assert!(!got.missing.iter().any(|m| m.object.contains("seq_ok")));
    assert_eq!(got.ddl_tables, vec!["sql/ddl/seq.sql".to_string()]);
}

#[test]
fn should_list_dynamic_raw_without_recording_it_missing() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/sql/r.sql","line":1}"#,
        r#"{"source":1,"target":1,"type":"dynamic","raw_expr":"PlVariable([\"v_sql\"])","file":"/proj/sql/r.sql","line":0}"#,
    );
    let got = close(&json, "caller", "/proj", &[]).unwrap();
    assert_eq!(got.dynamic.len(), 1);
    assert_eq!(got.dynamic[0].raw, r#"PlVariable(["v_sql"])"#);
    assert_eq!(got.dynamic[0].from, "caller");
    assert_eq!(got.dynamic[0].at, "sql/r.sql:0");
    assert!(got.missing.is_empty(), "{:?}", got.missing);
}

#[test]
fn should_not_miss_explicit_false_table_when_supplied_ddl_has_the_name() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"table","name":"t_have"}"#,
        r#"{"source":1,"target":2,"type":"table_access","modes":["read"],"write_kinds":[],"file":"/proj/a.sql","line":1}"#,
    );
    let got = close(
        &json,
        "caller",
        "/proj",
        &[("t_have", "sql/ddl/t_have.sql")],
    )
    .unwrap();
    assert!(got.missing.is_empty(), "{:?}", got.missing);
    assert_eq!(got.ddl_tables, vec!["sql/ddl/t_have.sql".to_string()]);
    assert_eq!(got.refs[0].object, "t_have");
}

#[test]
fn should_not_miss_system_table_or_engine_provided_tail_and_keep_them_out_of_ddl() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"table","name":"t_sys","system":true,"explicit":false},
           {"id":3,"type":"view","schema":"other","name":"sys_dummy","explicit":false},
           {"id":4,"type":"table","name":"t_need","explicit":true}"#,
        r#"{"source":1,"target":2,"type":"table_access","modes":["read"],"write_kinds":[],"file":"/proj/a.sql","line":1},
           {"source":1,"target":3,"type":"table_access","modes":["lock_read"],"write_kinds":[],"file":"/proj/a.sql","line":2},
           {"source":1,"target":4,"type":"table_access","modes":["read"],"write_kinds":[],"file":"/proj/a.sql","line":3}"#,
    );
    let got = close(&json, "caller", "/proj", &[]).unwrap();
    assert!(got.missing.iter().all(|m| m.object == "relation:t_need"));
    assert_eq!(got.missing.len(), 1);
    assert!(got.ddl_tables.is_empty(), "{:?}", got.ddl_tables);
    assert!(got
        .refs
        .iter()
        .any(|r| r.object == "t_sys" && r.role == "read"));
    assert!(got
        .refs
        .iter()
        .any(|r| r.object == "other.sys_dummy" && r.role == "read"));
}

#[test]
fn should_miss_materialized_view_only_when_project_ddl_is_absent() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/proj/a.sql","line":1},
           {"id":2,"type":"materialized_view","name":"mv_miss","schema":null,"file":"/elsewhere/mv.sql","line":1},
           {"id":3,"type":"materialized_view","name":"mv_ok","file":"/elsewhere/mv2.sql","line":1}"#,
        r#"{"source":1,"target":2,"type":"table_access","modes":["read"],"write_kinds":[],"file":"/proj/a.sql","line":1},
           {"source":1,"target":3,"type":"depends_on","file":"/proj/a.sql","line":2}"#,
    );
    let got = close(&json, "caller", "/proj", &[("mv_ok", "sql/views/mv.sql")]).unwrap();
    assert!(got
        .refs
        .iter()
        .any(|r| { r.object == "mv_miss" && r.kind == "view" && r.role == "read" }));
    assert!(got.refs.iter().any(|r| {
        r.object == "mv_ok" && r.kind == "view" && r.role == "read" && r.from == "caller"
    }));
    assert_eq!(got.missing.len(), 1);
    assert_eq!(got.missing[0].object, "relation:mv_miss");
    assert_eq!(got.ddl_tables, vec!["sql/views/mv.sql".to_string()]);
}

#[test]
fn should_build_catalog_keys_from_package_object_name_without_normalize() {
    let cases = [
        (
            r#"{"id":1,"type":"package","schema":"schema","name":"pkg","file":"/proj/p.sql","line":1},
               {"id":2,"type":"procedure","name":"proc","schema":"schema","file":"/proj/p.sql","line":2}"#,
            r#"{"source":1,"target":2,"type":"contains_routine"}"#,
            "schema.pkg.proc",
            "pkg.proc",
        ),
        (
            r#"{"id":1,"type":"package","schema":"db.sch","name":"pkg","file":"/proj/p.sql","line":1},
               {"id":2,"type":"function","name":"proc","schema":"db.sch","file":"/proj/p.sql","line":2}"#,
            r#"{"source":1,"target":2,"type":"contains_routine"}"#,
            "db.sch.pkg.proc",
            "pkg.proc",
        ),
        (
            r#"{"id":1,"type":"package","name":"pkg","file":"/proj/p.sql","line":1},
               {"id":2,"type":"procedure","name":"proc","file":"/proj/p.sql","line":2}"#,
            r#"{"source":1,"target":2,"type":"contains_routine"}"#,
            "pkg.proc",
            "proc",
        ),
        (
            r#"{"id":2,"type":"function","name":"func","schema":"db.schema","file":"/proj/f.sql","line":1}"#,
            "",
            "db.schema.func",
            "schema.func",
        ),
        (
            r#"{"id":2,"type":"procedure","name":"func","file":"/proj/f.sql","line":1}"#,
            "",
            "func",
            "other.func",
        ),
    ];
    for (nodes, edges, hit, miss) in cases {
        let json = doc(nodes, edges);
        let got = close(&json, hit, "/proj", &[]).unwrap();
        assert_eq!(got.target, hit);
        let err = close(&json, miss, "/proj", &[]).expect_err(miss);
        assert_eq!(err.code.to_string(), "BIZ-0150");
    }
}

#[test]
fn should_register_package_body_member_under_the_fixture_catalog_key() {
    let json = doc(
        r#"{"id":1,"type":"package","schema":"schema","name":"pkg","file":"/proj/p.sql","line":1},
           {"id":2,"type":"procedure","name":"proc","schema":"schema","file":"/proj/p.sql","line":2}"#,
        r#"{"source":1,"target":2,"type":"contains_routine"}"#,
    );
    let got = close(&json, "SCHEMA.PKG.PROC", "/proj", &[]).unwrap();
    let mut eng = Engine::new(Box::new(NoopExecutor), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PACKAGE BODY schema.pkg IS\n\
         PROCEDURE proc IS\n\
         BEGIN\n\
           NULL;\n\
         END;\n\
         END;\n/\n",
    )
    .expect("package body registers");
    let found = eng.catalog.lookup("schema.pkg.proc");
    assert!(found.is_some());
    assert!(eng.catalog.lookup("pkg.proc").is_none());
    assert_eq!(found.unwrap().name, got.target);
    assert_eq!(got.target, "schema.pkg.proc");
}

#[test]
fn should_strip_entry_file_into_relative_ddl_routines() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/work/sql/routines/A.sql","line":1},
           {"id":2,"type":"function","name":"inner","schema":"pkg","file":"/work/sql/routines/B.sql","line":4}"#,
        r#"{"source":1,"target":2,"type":"direct","scope":"pl","file":"/work/sql/routines/A.sql","line":0}"#,
    );
    let got = close(&json, "caller", "/work", &[]).unwrap();
    assert_eq!(
        got.ddl_routines,
        vec![
            "sql/routines/A.sql".to_string(),
            "sql/routines/B.sql".to_string()
        ]
    );
    assert_eq!(got.refs.len(), 1);
    assert_eq!(got.refs[0].object, "pkg.inner");
    assert_eq!(got.refs[0].kind, "routine");
    assert_eq!(got.refs[0].role, "call");
    assert_eq!(got.refs[0].from, "caller");
    assert_eq!(got.refs[0].at, "sql/routines/A.sql:0");
    assert!(got.missing.is_empty(), "{:?}", got.missing);
}

#[test]
fn should_miss_non_entry_outside_root_and_still_walk_its_out_edges() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/work/sql/A.sql","line":1},
           {"id":2,"type":"procedure","name":"outer","file":"/other/B.sql","line":1},
           {"id":3,"type":"unresolved","raw_expr":"fnc_missing","context":"call"},
           {"id":4,"type":"table","name":"t_out","explicit":true}"#,
        r#"{"source":1,"target":2,"type":"direct","scope":"pl","file":"/work/sql/A.sql","line":3},
           {"source":2,"target":3,"type":"direct","scope":"sql","file":"/other/B.sql","line":0},
           {"source":2,"target":4,"type":"table_access","modes":["read"],"write_kinds":[],"file":"/other/B.sql","line":4}"#,
    );
    let got = close(&json, "caller", "/work", &[]).unwrap();
    assert_eq!(got.ddl_routines, vec!["sql/A.sql".to_string()]);
    assert!(got.missing.iter().any(|m| {
        m.object == "routine:outer"
            && m.from == "caller"
            && m.at == "sql/A.sql:3"
            && !m.at.starts_with('/')
    }));
    assert!(got
        .missing
        .iter()
        .any(|m| { m.object == "routine:fnc_missing" && m.from == "outer" && m.at == "B.sql:0" }));
    assert!(got.refs.iter().any(|r| {
        r.object == "outer" && r.kind == "routine" && r.role == "call" && r.from == "caller"
    }));
    assert!(got.refs.iter().any(|r| {
        r.object == "t_out" && r.kind == "table" && r.role == "read" && r.from == "outer"
    }));
    assert!(got
        .missing
        .iter()
        .any(|m| m.object == "relation:t_out" && m.from == "outer"));
    assert!(got.refs.iter().all(|r| !r.at.starts_with('/')));
    assert!(got.ddl_routines.iter().all(|p| !p.starts_with('/')));
}

#[test]
fn should_keep_an_already_relative_edge_file_without_parent_segments() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/work/sql/routines/A.sql","line":1},
           {"id":2,"type":"unresolved","raw_expr":"fnc_x","context":"call"}"#,
        r#"{"source":1,"target":2,"type":"direct","scope":"sql","file":"sql/routines/B.sql","line":12}"#,
    );
    let got = close(&json, "caller", "/work", &[]).unwrap();
    assert_eq!(got.missing.len(), 1, "{:?}", got.missing);
    assert_eq!(got.missing[0].object, "routine:fnc_x");
    assert_eq!(got.missing[0].from, "caller");
    assert_eq!(got.missing[0].at, "sql/routines/B.sql:12");
}

#[test]
fn should_reject_entry_whose_file_is_outside_the_project_root() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"caller","file":"/other/A.sql","line":1}"#,
        "",
    );
    let err = close(&json, "caller", "/work", &[]).expect_err("entry outside");
    assert_eq!(err.code.to_string(), "ENV-0030");
    assert!(
        err.message.starts_with("入口源文件不在项目根下:"),
        "{}",
        err.message
    );
}

#[test]
fn should_reject_two_distinct_package_endpoints() {
    let json = doc(
        r#"{"id":1,"type":"package","schema":"s1","name":"pkg","file":"/proj/a.sql","line":1},
           {"id":2,"type":"package","schema":"s2","name":"pkg","file":"/proj/b.sql","line":1},
           {"id":3,"type":"procedure","name":"proc","schema":"s1","file":"/proj/p.sql","line":1}"#,
        r#"{"source":1,"target":3,"type":"contains_routine"},
           {"source":2,"target":3,"type":"contains_routine"}"#,
    );
    let err = close(&json, "s1.pkg.proc", "/proj", &[]).expect_err("two packages");
    assert_eq!(err.code.to_string(), "ENV-0030");
    assert!(
        err.message.starts_with("codeweb 图不自洽:"),
        "{}",
        err.message
    );
}

#[test]
fn should_accept_a_single_contains_routine_edge() {
    let json = doc(
        r#"{"id":1,"type":"package","schema":"schema","name":"pkg","file":"/proj/p.sql","line":1},
           {"id":2,"type":"procedure","name":"proc","schema":"schema","file":"/proj/p.sql","line":2}"#,
        r#"{"source":1,"target":2,"type":"contains_routine"}"#,
    );
    let got = close(&json, "schema.pkg.proc", "/proj", &[]).unwrap();
    assert_eq!(got.target, "schema.pkg.proc");
    assert!(got.missing.is_empty());
    assert_eq!(got.ddl_routines, vec!["p.sql".to_string()]);
}

#[test]
fn should_reject_when_the_entry_catalog_key_is_not_unique() {
    let json = doc(
        r#"{"id":1,"type":"procedure","name":"proc","schema":"sch","file":"/proj/a.sql","line":1},
           {"id":2,"type":"function","name":"proc","schema":"sch","file":"/proj/b.sql","line":1}"#,
        "",
    );
    let err = close(&json, "sch.proc", "/proj", &[]).expect_err("duplicate key");
    assert_eq!(err.code.to_string(), "ENV-0030");
    assert!(
        err.message.starts_with("入口在 codeweb 图中不唯一:"),
        "{}",
        err.message
    );
}
