//! `src/plan/` 单测：计划的字节口径（纯函数，不触碰内核与文件系统）。

use super::*;
use crate::plan::MissingObject;
use std::path::Path;

pub(super) fn plan_with_two_routines_and_a_gap() -> RunPlan {
    RunPlan {
        // 故意乱序：字节可复现要求渲染前排序
        ddl_routines: vec![
            "sql/routines/b_second.sql".into(),
            "sql/routines/a_first.sql".into(),
        ],
        ddl_tables: vec!["sql/ddl/tables.sql".into()],
        materialize: vec![],
        writeback: vec![],
        refs: vec![],
        dynamic: vec![],
        missing: vec![
            MissingObject {
                object: "relation:par_oper_close_date".into(),
                from: "pkg_a.fnc_x".into(),
            },
            MissingObject {
                object: "sequence:s_order".into(),
                from: "pkg_a.prc_main".into(),
            },
        ],
        target: "pkg_a.prc_main".into(),
        generator: "spinduck test (plan schema 1)".into(),
        project_dir: "../..".into(),
        schema: Some("bigfund".into()),
    }
}

#[test]
fn should_render_plan_keys_and_sorted_lists_independent_of_input_order() {
    let text = plan_with_two_routines_and_a_gap().to_toml_string();
    assert!(text.contains("schema_version = 1\n"), "{text}");
    assert!(text.contains("target = \"pkg_a.prc_main\"\n"), "{text}");
    assert!(text.contains("schema = \"bigfund\"\n"), "{text}");
    assert!(text.contains("project_dir = \"../..\"\n"), "{text}");
    assert!(
        text.contains(
            "ddl_routines = [\"sql/routines/a_first.sql\", \"sql/routines/b_second.sql\"]"
        ),
        "清单须排序且同表内联: {text}"
    );
    assert!(
        text.contains("ddl_tables = [\"sql/ddl/tables.sql\"]"),
        "{text}"
    );
    assert!(text.contains("materialize = []\n"), "{text}");
    assert!(text.contains("writeback = []\n"), "{text}");
}

#[test]
fn should_render_each_missing_object_with_its_referrer() {
    let text = plan_with_two_routines_and_a_gap().to_toml_string();
    assert!(
        text.contains(
            "[[missing]]\nobject = \"relation:par_oper_close_date\"\nfrom = \"pkg_a.fnc_x\"\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("[[missing]]\nobject = \"sequence:s_order\"\nfrom = \"pkg_a.prc_main\"\n"),
        "{text}"
    );
}

#[test]
fn should_omit_schema_key_when_plan_carries_no_schema() {
    let mut p = plan_with_two_routines_and_a_gap();
    p.schema = None;
    let text = p.to_toml_string();
    assert!(!text.contains("schema = "), "缺省不该写空 schema: {text}");
    assert!(text.contains("schema_version = 1\n"), "{text}");
}

#[test]
fn should_render_identical_bytes_for_the_same_plan_twice() {
    // §6.1 的承诺本体：同输入同字节（计划可复现），与进程状态无关。
    let a = plan_with_two_routines_and_a_gap().to_toml_string();
    let b = plan_with_two_routines_and_a_gap().to_toml_string();
    assert_eq!(a, b);
    assert_eq!(a, LEGACY_GAP_PLAN);
}

const LEGACY_GAP_PLAN: &str = "[plan]
schema_version = 1
target = \"pkg_a.prc_main\"
generator = \"spinduck test (plan schema 1)\"
project_dir = \"../..\"
schema = \"bigfund\"

[sources]
ddl_routines = [\"sql/routines/a_first.sql\", \"sql/routines/b_second.sql\"]
ddl_tables = [\"sql/ddl/tables.sql\"]
materialize = []
writeback = []

[[missing]]
object = \"relation:par_oper_close_date\"
from = \"pkg_a.fnc_x\"

[[missing]]
object = \"sequence:s_order\"
from = \"pkg_a.prc_main\"
";

#[test]
fn should_read_back_every_field_it_rendered() {
    // `deploy`（A2-3）的输入面：读侧必须还原写侧的全部字段，否则计划只是"给人看的清单"。
    let src = plan_with_two_routines_and_a_gap();
    let back = RunPlan::from_toml_str(&src.to_toml_string()).unwrap();
    assert_eq!(back.target, src.target);
    assert_eq!(back.generator, src.generator);
    assert_eq!(back.project_dir, src.project_dir);
    assert_eq!(back.schema, src.schema);
    assert_eq!(
        back.ddl_routines,
        vec![
            "sql/routines/a_first.sql".to_string(),
            "sql/routines/b_second.sql".to_string(),
        ],
        "读回的清单即渲染口径（排序去重）"
    );
    assert_eq!(back.ddl_tables, src.ddl_tables);
    assert_eq!(
        back.missing,
        vec![
            MissingObject {
                object: "relation:par_oper_close_date".into(),
                from: "pkg_a.fnc_x".into(),
            },
            MissingObject {
                object: "sequence:s_order".into(),
                from: "pkg_a.prc_main".into(),
            },
        ]
    );
}

#[test]
fn should_read_back_a_plan_without_schema_or_missing() {
    let mut src = plan_with_two_routines_and_a_gap();
    src.schema = None;
    src.missing.clear();
    let back = RunPlan::from_toml_str(&src.to_toml_string()).unwrap();
    assert_eq!(back.schema, None);
    assert!(back.missing.is_empty());
    assert_eq!(back.materialize, Vec::<String>::new());
    assert_eq!(back.writeback, Vec::<String>::new());
}

#[test]
fn should_reject_a_plan_from_an_unknown_schema_version() {
    // 版本不符就猜字段含义 = 用错的口径建环境；读侧一律先拒。
    let text = plan_with_two_routines_and_a_gap().to_toml_string();
    let foreign = text.replace("schema_version = 1", "schema_version = 2");
    let e = RunPlan::from_toml_str(&foreign).unwrap_err();
    assert_eq!(e.code.to_string(), "ENV-0030");
    assert!(
        e.message.contains("schema_version=2") && e.message.contains("只认 1"),
        "诊断要给两侧版本号：{}",
        e.message
    );
}

#[test]
fn should_reject_a_plan_missing_a_required_field() {
    let text = plan_with_two_routines_and_a_gap().to_toml_string();
    let no_target = text.replace("target = \"pkg_a.prc_main\"\n", "");
    let e = RunPlan::from_toml_str(&no_target).unwrap_err();
    assert_eq!(e.code.to_string(), "ENV-0030");
    assert!(
        e.message.contains("[plan].target"),
        "诊断要点名缺的字段：{}",
        e.message
    );
}

// —— 闭包求值（§6.1 / R3）：项目夹具走真实文件系统，例程只编译不执行（NoopExecutor）。

fn write_rel(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
}

const PROJECT_TOML: &str = "[project]\nname = \"plan_fixture\"\nkernel = \"1.5.5\"\n\n[paths]\nddl = \"sql/ddl\"\nroutines = \"sql/routines\"\n";

const MAIN_ROUTINE: &str = "CREATE OR REPLACE PROCEDURE prc_main IS
  v_n INT;
BEGIN
  CALL prc_child(1);
  CALL pkg_a.fnc_child(2);
  SELECT COUNT(*) INTO v_n FROM t_a;
  INSERT INTO t_b (id) VALUES (seq_b.nextval);
END;
/
";

/// `prc_child`（独立过程）与 `pkg_a.fnc_child`（包体内过程）同文件：
/// 闭包必须同时认得两种归属形态。
const CHILD_ROUTINE: &str = "CREATE OR REPLACE PROCEDURE prc_child(p_id IN INT) IS
BEGIN
  NULL;
END;
/

CREATE OR REPLACE PACKAGE BODY pkg_a IS
  PROCEDURE fnc_child(p_id IN INT) IS
  BEGIN
    UPDATE t_c SET id = p_id;
  END;
END;
/
";

const FULL_DDL: &str = "CREATE TABLE t_a (id INT);
CREATE TABLE t_b (id INT);
CREATE TABLE t_c (id INT);
CREATE SEQUENCE seq_b START WITH 1;
";

/// 写入两套例程 + 指定 DDL 的最小项目；返回项目根。
fn fixture_project(root: &Path, ddl: &str) {
    write_rel(root, "spinduck.toml", PROJECT_TOML);
    write_rel(root, "sql/routines/10_main.sql", MAIN_ROUTINE);
    write_rel(root, "sql/routines/20_child.sql", CHILD_ROUTINE);
    write_rel(root, "sql/ddl/tables.sql", ddl);
}

fn plan_for(root: &Path, target: &str) -> crate::plan::RunPlan {
    crate::plan::build(
        &crate::project::ProjectConfig::load(root).unwrap(),
        target,
        None,
    )
    .unwrap()
}

#[test]
fn should_collect_routine_and_ddl_files_transitively_into_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    fixture_project(tmp.path(), FULL_DDL);
    let plan = plan_for(tmp.path(), "prc_main");

    assert_eq!(plan.target, "prc_main");
    assert_eq!(
        plan.ddl_routines,
        vec![
            "sql/routines/10_main.sql".to_string(),
            "sql/routines/20_child.sql".to_string(),
        ],
        "两级调用（独立过程 + 包体内过程）的源文件都要进计划"
    );
    assert_eq!(plan.ddl_tables, vec!["sql/ddl/tables.sql".to_string()]);
    assert!(plan.missing.is_empty(), "缺件表应为空: {:?}", plan.missing);
    assert_eq!(
        plan.materialize,
        Vec::<String>::new(),
        "§6.4：物化清单默认空"
    );
    assert_eq!(
        plan.writeback,
        Vec::<String>::new(),
        "§6.4：回写白名单默认空"
    );
    assert_eq!(
        plan.project_dir, "../..",
        "相对计划文件所在目录，不含绝对路径"
    );
    assert_eq!(plan.schema, None);
}

#[test]
fn should_record_a_referenced_table_with_no_declaring_ddl_as_missing() {
    let tmp = tempfile::tempdir().unwrap();
    // t_c 只在包体过程的 UPDATE 里出现，DDL 不建它
    fixture_project(
        tmp.path(),
        "CREATE TABLE t_a (id INT);
CREATE TABLE t_b (id INT);
CREATE SEQUENCE seq_b START WITH 1;
",
    );
    let plan = plan_for(tmp.path(), "prc_main");
    assert_eq!(
        plan.missing,
        vec![MissingObject {
            object: "relation:t_c".into(),
            from: "pkg_a.fnc_child".into(),
        }],
        "缺件要带引用它的例程（R3 定位）"
    );
}

#[test]
fn should_record_a_sequence_with_no_declaring_ddl_as_missing() {
    let tmp = tempfile::tempdir().unwrap();
    fixture_project(
        tmp.path(),
        "CREATE TABLE t_a (id INT);
CREATE TABLE t_b (id INT);
CREATE TABLE t_c (id INT);
",
    );
    let plan = plan_for(tmp.path(), "prc_main");
    assert_eq!(
        plan.missing,
        vec![MissingObject {
            object: "sequence:seq_b".into(),
            from: "prc_main".into(),
        }],
        "seq_b.nextval 改写为 nextval('seq_b') 后仍要进闭包"
    );
}

#[test]
fn should_record_a_called_routine_that_no_file_declares_as_missing() {
    let tmp = tempfile::tempdir().unwrap();
    write_rel(tmp.path(), "spinduck.toml", PROJECT_TOML);
    write_rel(
        tmp.path(),
        "sql/routines/10_main.sql",
        "CREATE OR REPLACE PROCEDURE prc_main IS
BEGIN
  CALL pkg_z.prc_ghost(1);
END;
/
",
    );
    let plan = plan_for(tmp.path(), "prc_main");
    assert_eq!(
        plan.missing,
        vec![MissingObject {
            object: "routine:pkg_z.prc_ghost".into(),
            from: "prc_main".into(),
        }]
    );
    assert_eq!(
        plan.ddl_routines,
        vec!["sql/routines/10_main.sql".to_string()],
        "缺失的被调例程不该混进源文件清单"
    );
}

#[test]
fn should_fail_when_the_target_routine_is_not_declared_by_the_project() {
    let tmp = tempfile::tempdir().unwrap();
    fixture_project(tmp.path(), FULL_DDL);
    let project = crate::project::ProjectConfig::load(tmp.path()).unwrap();
    let e = crate::plan::build(&project, "prc_nope", None).unwrap_err();
    assert_eq!(
        e.code.to_string(),
        "BIZ-0150",
        "复用既有\"例程未登记\"错误码"
    );
    assert!(e.message.contains("prc_nope"), "{}", e.message);
}

#[test]
fn should_resolve_a_bare_sibling_call_inside_the_same_package() {
    // 语料现实（考题包体）：同包兄弟过程按裸名调用。口径必须与 `verify` 一致——
    // 先按调用方所属包补全，再试裸名，否则计划会对真实语料报假缺件。
    let tmp = tempfile::tempdir().unwrap();
    write_rel(tmp.path(), "spinduck.toml", PROJECT_TOML);
    write_rel(
        tmp.path(),
        "sql/routines/10_pkg.sql",
        "CREATE OR REPLACE PACKAGE BODY pkg_a IS
  PROCEDURE prc_helper(p_id IN INT) IS
  BEGIN
    UPDATE t_b SET id = p_id;
  END;
  PROCEDURE prc_main IS
    v_n INT;
  BEGIN
    prc_helper(1);
    SELECT COUNT(*) INTO v_n FROM t_a;
  END;
END;
/
",
    );
    write_rel(tmp.path(), "sql/ddl/tables.sql", FULL_DDL);
    let plan = plan_for(tmp.path(), "pkg_a.prc_main");
    assert!(
        plan.missing.is_empty(),
        "裸名同包调用不该落缺件: {:?}",
        plan.missing
    );
    assert_eq!(
        plan.ddl_tables,
        vec!["sql/ddl/tables.sql".to_string()],
        "兄弟过程的表依赖要传递进闭包"
    );
}

#[test]
fn should_match_a_view_declared_behind_an_inline_comment() {
    // 语料现实（`sql/ddl/views/1_V_*.sql`）：`CREATE OR REPLACE /*FORCE*/ VIEW …`。
    // 词法扫描把注释当词元就会漏登记这张视图，计划随即对真实项目报假缺件。
    let tmp = tempfile::tempdir().unwrap();
    write_rel(tmp.path(), "spinduck.toml", PROJECT_TOML);
    write_rel(
        tmp.path(),
        "sql/routines/10_main.sql",
        "CREATE OR REPLACE PROCEDURE prc_main IS
BEGIN
  DELETE FROM v_par_bond WHERE id = 1;
END;
/
",
    );
    write_rel(
        tmp.path(),
        "sql/ddl/views/1_V_PAR_BOND.sql",
        "drop view if exists V_PAR_BOND;\nCREATE OR REPLACE /*FORCE*/ VIEW V_PAR_BOND (id) AS\n  SELECT 1 FROM dual;\n",
    );
    let plan = plan_for(tmp.path(), "prc_main");
    assert!(
        plan.missing.is_empty(),
        "注释后的视图声明要能登记: {:?}",
        plan.missing
    );
    assert_eq!(
        plan.ddl_tables,
        vec!["sql/ddl/views/1_V_PAR_BOND.sql".to_string()]
    );
}

#[test]
fn should_include_the_base_table_that_a_planned_view_reads() {
    // 视图进了部署清单、它读的表却没进：闭包只从例程出发，视图体不在遍历面内。
    // 后果不是报错而是静默降级——`deploy` 建视图时内核缺表，退化成 `SELECT 1` 占位视图，
    // 对象在而列不在（实测：读该视图列的 `call` 得到 Binder Error，候选列只有 "1"）。
    let tmp = tempfile::tempdir().unwrap();
    write_rel(tmp.path(), "spinduck.toml", PROJECT_TOML);
    write_rel(
        tmp.path(),
        "sql/routines/10_main.sql",
        "CREATE OR REPLACE PROCEDURE prc_main IS
  v_n INT;
BEGIN
  SELECT COUNT(*) INTO v_n FROM v_bond;
END;
/
",
    );
    write_rel(
        tmp.path(),
        "sql/ddl/views/1_V_BOND.sql",
        "CREATE OR REPLACE /*FORCE*/ VIEW v_bond (id) AS SELECT id FROM t_base;\n",
    );
    write_rel(
        tmp.path(),
        "sql/ddl/tables/t_base.sql",
        "CREATE TABLE t_base (id INT);\n",
    );
    let plan = plan_for(tmp.path(), "prc_main");
    assert_eq!(
        plan.ddl_tables,
        vec![
            "sql/ddl/tables/t_base.sql".to_string(),
            "sql/ddl/views/1_V_BOND.sql".to_string(),
        ],
        "视图体的基表要随视图一起进清单（下发顺序才可能表先于视图）"
    );
    assert!(plan.missing.is_empty(), "{:?}", plan.missing);
}

#[test]
fn should_not_report_an_engine_provided_table_as_missing() {
    // `sys_dummy` 由 `deploy_schema` 无条件建（`verify/deploy.rs`），项目里没有它的 DDL 文件。
    // 静态闭包若按"无声明即缺件"处理，每个用它做常量取数的过程都会报假缺件；
    // 它也没有源文件，所以既不进 ddl_tables 也不进 missing。
    let tmp = tempfile::tempdir().unwrap();
    write_rel(tmp.path(), "spinduck.toml", PROJECT_TOML);
    write_rel(
        tmp.path(),
        "sql/routines/10_main.sql",
        "CREATE OR REPLACE PROCEDURE prc_main IS
  v_n INT;
BEGIN
  SELECT COUNT(*) INTO v_n FROM sys_dummy;
END;
/
",
    );
    write_rel(
        tmp.path(),
        "sql/ddl/tables.sql",
        "CREATE TABLE t_a (id INT);\n",
    );
    let plan = plan_for(tmp.path(), "prc_main");
    assert!(
        plan.missing.is_empty(),
        "内核自建对象不该落缺件: {:?}",
        plan.missing
    );
    assert!(
        plan.ddl_tables.is_empty(),
        "没有源文件的对象不该进部署清单: {:?}",
        plan.ddl_tables
    );
}

#[test]
fn should_match_a_schema_qualified_reference_against_a_bare_ddl_name() {
    // 语料写 `bigfund.t_a`、DDL 声明裸名 `t_a`：与 verify 的 present() 同口径（先全名后尾段）。
    let tmp = tempfile::tempdir().unwrap();
    write_rel(tmp.path(), "spinduck.toml", PROJECT_TOML);
    write_rel(
        tmp.path(),
        "sql/routines/10_main.sql",
        "CREATE OR REPLACE PROCEDURE prc_main IS
BEGIN
  DELETE FROM bigfund.t_a WHERE id = 1;
END;
/
",
    );
    write_rel(
        tmp.path(),
        "sql/ddl/tables.sql",
        "CREATE TABLE t_a (id INT);\n",
    );
    let plan = plan_for(tmp.path(), "prc_main");
    assert!(
        plan.missing.is_empty(),
        "尾段命中即不是缺件: {:?}",
        plan.missing
    );
    assert_eq!(plan.ddl_tables, vec!["sql/ddl/tables.sql".to_string()]);
}
