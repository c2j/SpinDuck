//! `plan`：运行计划生成（design §6.1 / §7 A2）。
//!
//! 契约三件事：干净闭包 → rc=0 且计划落盘；有缺件 → rc=1 且逐条列出；
//! 用法/目标错 → rc=2 / rc=1。计划文件字节必须可复现（同输入两次跑逐字节相同）。

use super::{abs_file, cli, cw_graph, plan_in_process, write, EXIT_FAIL, EXIT_OK, EXIT_USAGE};

const PROJECT_PLAN: &str = "[project]\nname = \"p\"\nkernel = \"1.5.5\"\n\n[paths]\nddl = \"sql/ddl\"\nroutines = \"sql/routines\"\nhome = \".spinduck\"\n";

/// 两级引用：`prc_main` 调 `prc_child`（同文件）并读 `t_a`。
const ROUTINES_PLAN: &str = "CREATE OR REPLACE PROCEDURE prc_child(p_id IN INT) IS
BEGIN
  NULL;
END;
/

CREATE OR REPLACE PROCEDURE prc_main IS
  v_n INT;
BEGIN
  CALL prc_child(1);
  SELECT COUNT(*) INTO v_n FROM t_a;
END;
/
";

const DDL_T_A: &str = "CREATE TABLE t_a (id INT);\n";
const DDL_EMPTY: &str = "-- 什么都没建（缺件面的夹具）\n";

/// 写出一个最小可计划项目，返回项目根所在临时目录（持有所有权直到用例结束）。
fn plan_project(dir: &std::path::Path, ddl: &str) {
    write(dir, "spinduck.toml", PROJECT_PLAN);
    write(dir, "sql/routines/10_main.sql", ROUTINES_PLAN);
    write(dir, "sql/ddl/tables.sql", ddl);
}

// ------------------------------------------------------------ A2-5：表达式级被调函数

/// 表达式里的裸名函数调用（`v_n := fnc_hidden()`）：被调函数与**它自己的表**都在闭包里。
const ROUTINES_EXPR_CALL: &str = "CREATE OR REPLACE PROCEDURE prc_outer IS
  v_n INT;
BEGIN
  v_n := fnc_hidden();
END;
/
";
/// 被调函数单独一个文件：闭包扩到它时，源文件清单必须跟着多一条（可断言的落点）。
const ROUTINE_FNC_HIDDEN: &str = "CREATE OR REPLACE FUNCTION fnc_hidden RETURN INT IS
  v INT;
BEGIN
  SELECT COUNT(*) INTO v FROM t_hidden;
  RETURN v;
END;
/
";
/// 表达式级的内建/库函数调用：目录里没有这个名字，按内核求值处理，不得据此报缺件。
const ROUTINES_EXPR_BUILTIN: &str = "CREATE OR REPLACE PROCEDURE prc_builtin IS
  v_n INT;
BEGIN
  v_n := trunc(1);
END;
/
";

fn expr_call_project(dir: &std::path::Path, ddl: &str) {
    write(dir, "spinduck.toml", PROJECT_PLAN);
    write(dir, "sql/routines/10_outer.sql", ROUTINES_EXPR_CALL);
    write(dir, "sql/routines/20_hidden.sql", ROUTINE_FNC_HIDDEN);
    write(dir, "sql/ddl/tables.sql", ddl);
}

#[test]
fn should_put_an_expression_called_function_and_its_own_table_in_the_closure() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    expr_call_project(root, DDL_EMPTY);
    let outer = abs_file(&root.join("sql/routines/10_outer.sql"));
    let hidden = abs_file(&root.join("sql/routines/20_hidden.sql"));
    let (code, out) = plan_in_process(
        root,
        "prc_outer",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_outer", "file": outer},
                {"id": 2, "type": "function", "name": "fnc_hidden", "file": hidden},
                {"id": 3, "type": "table", "name": "t_hidden"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "direct", "file": outer, "line": 0},
                {"source": 2, "target": 3, "type": "table_access", "modes": ["read"], "file": hidden, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_FAIL, "被调函数的表没声明即缺件: {out}");
    assert!(
        out.contains("MISS relation:t_hidden <- fnc_hidden"),
        "缺件归属必须是**被调函数**（引用它的那一层），不是外层过程: {out}"
    );
    let text = std::fs::read_to_string(tmp.path().join(".spinduck/plan/prc_outer.toml")).unwrap();
    assert!(
        text.contains(
            "ddl_routines = [\"sql/routines/10_outer.sql\", \"sql/routines/20_hidden.sql\"]\n"
        ),
        "被调函数的源文件要进部署清单，否则 deploy 出来的库里没有这个函数: {text}"
    );
}

#[test]
fn should_deploy_the_expression_called_function_when_its_table_is_declared() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    expr_call_project(
        root,
        "CREATE TABLE t_a (id INT);\nCREATE TABLE t_hidden (k INT);\n",
    );
    let outer = abs_file(&root.join("sql/routines/10_outer.sql"));
    let hidden = abs_file(&root.join("sql/routines/20_hidden.sql"));
    let (code, out) = plan_in_process(
        root,
        "prc_outer",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_outer", "file": outer},
                {"id": 2, "type": "function", "name": "fnc_hidden", "file": hidden},
                {"id": 3, "type": "table", "name": "t_hidden"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "direct", "file": outer, "line": 0},
                {"source": 2, "target": 3, "type": "table_access", "modes": ["read"], "file": hidden, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains("plan: target=prc_outer routines=2 tables=1 missing=0"),
        "routines 计**源文件**数（含被调函数那个文件）；tables 同样计文件数: {out}"
    );
    let text = std::fs::read_to_string(tmp.path().join(".spinduck/plan/prc_outer.toml")).unwrap();
    assert!(
        text.contains("ddl_tables = [\"sql/ddl/tables.sql\"]\n"),
        "{text}"
    );
}

#[test]
fn should_not_report_a_missing_object_for_a_builtin_expression_call() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_PLAN);
    write(
        tmp.path(),
        "sql/routines/10_builtin.sql",
        ROUTINES_EXPR_BUILTIN,
    );
    write(tmp.path(), "sql/ddl/tables.sql", DDL_T_A);
    let root = tmp.path();
    let file = abs_file(&root.join("sql/routines/10_builtin.sql"));
    let (code, out) = plan_in_process(
        root,
        "prc_builtin",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_builtin", "file": file},
                {"id": 2, "type": "builtin_function", "name": "trunc"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "uses_builtin_function", "file": file, "line": 0},
                {"source": 1, "target": 2, "type": "direct", "file": file, "line": 0}
            ]),
        ),
    );
    assert_eq!(
        code, EXIT_OK,
        "内建函数不是项目例程，不该凭空多一条缺件: {out}"
    );
    assert!(out.contains("missing=0"), "{out}");
    assert!(!out.contains("MISS"), "{out}");
}

#[test]
fn should_exit_0_and_write_the_plan_file_for_a_clean_closure() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    plan_project(root, DDL_T_A);
    let file = abs_file(&root.join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        root,
        "prc_main",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_main", "file": file},
                {"id": 2, "type": "procedure", "name": "prc_child", "file": file},
                {"id": 3, "type": "table", "name": "t_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "direct", "file": file, "line": 0},
                {"source": 1, "target": 3, "type": "table_access", "modes": ["read"], "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains("plan: target=prc_main routines=1 tables=1 missing=0"),
        "{out}"
    );

    let plan = tmp.path().join(".spinduck/plan/prc_main.toml");
    assert!(plan.is_file(), "计划须落到 <home>/plan/<例程>.toml: {out}");
    let text = std::fs::read_to_string(&plan).unwrap();
    assert!(text.starts_with("[plan]\nschema_version = 1\n"), "{text}");
    assert!(text.contains("project_dir = \"../..\"\n"), "{text}");
    assert!(
        text.contains("ddl_routines = [\"sql/routines/10_main.sql\"]\n"),
        "{text}"
    );
    assert!(
        text.contains("ddl_tables = [\"sql/ddl/tables.sql\"]\n"),
        "被引用的表要经 DDL 索引回到源文件: {text}"
    );
    assert!(text.contains("writeback = []\n"), "{text}");
    assert!(
        !text.contains("[[missing]]"),
        "干净闭包不该有缺件段: {text}"
    );
}

#[test]
fn should_exit_1_and_list_every_missing_object() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    plan_project(root, DDL_EMPTY);
    let file = abs_file(&root.join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        root,
        "prc_main",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_main", "file": file},
                {"id": 2, "type": "procedure", "name": "prc_child", "file": file},
                {"id": 3, "type": "table", "name": "t_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "direct", "file": file, "line": 0},
                {"source": 1, "target": 3, "type": "table_access", "modes": ["read"], "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_FAIL, "缺件即计划不可执行: {out}");
    assert!(
        out.contains("plan: target=prc_main routines=1 tables=0 missing=1"),
        "{out}"
    );
    assert!(out.contains("MISS relation:t_a <- prc_main"), "{out}");
    // 缺件同样进计划文件：deploy 侧读到的必须是同一份真相
    let text = std::fs::read_to_string(tmp.path().join(".spinduck/plan/prc_main.toml")).unwrap();
    assert!(
        text.contains("[[missing]]\nobject = \"relation:t_a\"\nfrom = \"prc_main\"\n"),
        "{text}"
    );
}

#[test]
fn should_write_byte_identical_plan_files_on_two_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    plan_project(root, DDL_T_A);
    let file = abs_file(&root.join("sql/routines/10_main.sql"));
    let json = cw_graph(
        serde_json::json!([
            {"id": 1, "type": "procedure", "name": "prc_main", "file": file},
            {"id": 2, "type": "procedure", "name": "prc_child", "file": file},
            {"id": 3, "type": "table", "name": "t_a"}
        ]),
        serde_json::json!([
            {"source": 1, "target": 2, "type": "direct", "file": file, "line": 0},
            {"source": 1, "target": 3, "type": "table_access", "modes": ["read"], "file": file, "line": 1}
        ]),
    );
    let (code1, out1) = plan_in_process(root, "prc_main", None, json.clone());
    let first = std::fs::read(root.join(".spinduck/plan/prc_main.toml")).unwrap();
    let (code2, out2) = plan_in_process(root, "prc_main", None, json);
    let second = std::fs::read(tmp.path().join(".spinduck/plan/prc_main.toml")).unwrap();
    assert_eq!((code1, code2), (EXIT_OK, EXIT_OK), "{out1}\n{out2}");
    assert_eq!(first, second, "§6.1：同一输入必须逐字节可复现");
    // 渲染里唯一允许变化的是落盘标注；其余两行必须逐字相同（不带时间戳之类易变字段）
    assert!(out1.contains("(updated)"), "{out1}");
    assert!(out2.contains("(unchanged)"), "{out2}");
    let strip = |s: &str| s.replace("(updated)", "()").replace("(unchanged)", "()");
    assert_eq!(strip(&out1), strip(&out2), "{out1}\n{out2}");
}

#[test]
fn should_honor_the_schema_override() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    plan_project(root, DDL_T_A);
    let file = abs_file(&root.join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        root,
        "prc_main",
        Some("BIGFUND"),
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_main", "file": file},
                {"id": 2, "type": "table", "name": "t_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "table_access", "modes": ["read"], "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "{out}");
    let text = std::fs::read_to_string(tmp.path().join(".spinduck/plan/prc_main.toml")).unwrap();
    // 名字一律归一小写后入计划（与链路抽取、内核目录同口径）
    assert!(text.contains("schema = \"bigfund\"\n"), "{text}");
    assert!(text.contains("project_dir = \"../..\"\n"), "{text}");
}

#[test]
fn should_exit_2_with_eng_0010_when_target_routine_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, out, _) = cli(&["plan", "--dir", &tmp.path().to_string_lossy()]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.starts_with("[ENG-0010] usage: plan"), "{out}");
}

#[test]
fn should_exit_1_with_biz_0150_when_target_routine_is_not_declared() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    plan_project(root, DDL_T_A);
    let file = abs_file(&root.join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        root,
        "prc_ghost",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_main", "file": file}
            ]),
            serde_json::json!([]),
        ),
    );
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[BIZ-0150] "), "{out}");
    assert!(out.contains("prc_ghost"), "{out}");
    assert!(
        !tmp.path().join(".spinduck/plan/prc_ghost.toml").exists(),
        "判红不得留下计划文件"
    );
}
