//! `deploy`：按运行计划把资产装进**落盘**的本地 DuckDB（design §6.2 / §7 A2-3）。
//!
//! 本层能证明而单测证明不了的两件事：库文件真的留在磁盘上，以及换一个进程只带
//! `--db` 就能在部署好的库上跑真题（`call` 走的是文件里已有的表，不是自己建的）。
//! 缺件面另两个契约（缺件即拒、缺文件即红）见本文件后半。

use super::{abs_file, cli, cw_graph, plan_in_process, write, EXIT_FAIL, EXIT_OK, EXIT_USAGE};

const PROJECT_DEPLOY: &str =
    "[project]\nname = \"d\"\nkernel = \"1.5.5\"\n\n[paths]\nddl = \"sql/ddl\"\nroutines = \"sql/routines\"\nhome = \".spinduck\"\n";

/// 真题形状：序列取值入表，再数回来——一条用例同时覆盖表、序列与落盘后的可写性。
const ROUTINE_SEED_COUNT: &str = "CREATE OR REPLACE PROCEDURE prc_seed_count(p_o OUT NUMBER) IS
BEGIN
  INSERT INTO t_a (id) VALUES (seq_a.nextval);
  SELECT COUNT(*) INTO p_o FROM t_a;
END;
/
";

const DDL_TABLE_AND_SEQ: &str = "CREATE TABLE t_a (id INT);\nCREATE SEQUENCE seq_a START WITH 1;\n";

/// 写出可计划、可部署的最小项目，并跑一次 `plan` 生成计划文件；返回计划路径。
fn deployed_plan(dir: &std::path::Path) -> std::path::PathBuf {
    write(dir, "spinduck.toml", PROJECT_DEPLOY);
    write(dir, "sql/routines/10_main.sql", ROUTINE_SEED_COUNT);
    write(dir, "sql/ddl/tables.sql", DDL_TABLE_AND_SEQ);
    let file = abs_file(&dir.join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        dir,
        "prc_seed_count",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_seed_count", "file": file},
                {"id": 2, "type": "table", "name": "t_a"},
                {"id": 3, "type": "sequence", "name": "seq_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "table_access", "modes": ["write"], "file": file, "line": 1},
                {"source": 1, "target": 3, "type": "uses_sequence", "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "夹具计划本身要干净: {out}");
    dir.join(".spinduck/plan/prc_seed_count.toml")
}

#[test]
fn should_leave_a_persisted_database_a_second_process_can_run_against() {
    let tmp = tempfile::tempdir().unwrap();
    let plan = deployed_plan(tmp.path());
    let db = tmp.path().join("work.duckdb");
    let (code, out, err) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(
        out.contains("deploy: target=prc_seed_count ddl_files=1 routine_files=1"),
        "{out}"
    );
    assert!(db.is_file(), "deploy 的产物是库文件，不是内存态: {out}");

    // 换一个进程：只给 --db，表与序列必须已经在库里（call 不建表）
    let routine = tmp.path().join("sql/routines/10_main.sql");
    let (code, out, _) = cli(&[
        "call",
        "--db",
        &db.to_string_lossy(),
        "--file",
        &routine.to_string_lossy(),
        "prc_seed_count",
        "0",
    ]);
    assert_eq!(code, EXIT_OK, "落盘库里缺表/缺序列就会红在这里: {out}");
    assert!(out.contains("OUT p_o = 1"), "{out}");

    // 再换一个进程：上一进程的写入也落进了文件（回写面的前置事实）
    let (code, out, _) = cli(&[
        "call",
        "--db",
        &db.to_string_lossy(),
        "--file",
        &routine.to_string_lossy(),
        "prc_seed_count",
        "0",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.contains("OUT p_o = 2"), "写入未落盘: {out}");
}

#[test]
fn should_call_the_plan_target_without_passing_any_source_file() {
    // A2-4（design D5/R3）：计划里既有目标例程也有例程源清单，所以 `call --plan` 不必再
    // 逐个 `--file`。这条同时是 V31.3 那条反面事实的解——例程注册在**进程内**目录，
    // 换一个进程只带 `--db` 会得到 `BIZ-0150 not registered`；带计划即由计划补回源码面。
    let tmp = tempfile::tempdir().unwrap();
    let plan = deployed_plan(tmp.path());
    let db = tmp.path().join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");

    let (code, out, err) = cli(&[
        "call",
        "--plan",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
        "0",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "OUT p_o = 1\n", "计划面调用要跑库里已有的表: {out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_apply_the_execution_flags_on_the_plan_face_too() {
    // 合并轮（V36）：`--batch-commit` / `--step-budget` 是主干侧的执行面开关，`--plan`
    // 是本分支的目标面入口。两者共用同一个解析循环与同一个 `call_cmd`，所以计划面也必须
    // 吃下这两个开关——否则"发版二进制按计划跑千万级真题"就没有放大步数预算的入口。
    let tmp = tempfile::tempdir().unwrap();
    let plan = deployed_plan(tmp.path());
    let db = tmp.path().join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");

    let (code, out, err) = cli(&[
        "call",
        "--plan",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
        "--batch-commit",
        "--step-budget",
        "1000000",
        "0",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(
        out, "OUT p_o = 1\n",
        "计划面带开关的输出要与不带时逐字相同: {out}"
    );
    assert_eq!(err, "", "全部输出走 stdout");

    // 用法判定排在计划 IO 之前：缺文件不得掩盖 `--step-budget 0`（对比上面
    // `env_0010_when_the_plan_file_given_to_call_is_absent`，同一条路径单给是 EXIT_FAIL）
    let bogus = tmp.path().join("nope.toml");
    let (code, out, _) = cli(&[
        "call",
        "--plan",
        &bogus.to_string_lossy(),
        "--step-budget",
        "0",
    ]);
    assert_eq!(code, EXIT_USAGE, "usage 要先于计划读取: {out}");
    assert!(out.contains("--step-budget"), "{out}");
    assert!(!out.contains("[ENV-0010]"), "{out}");
}

#[test]
fn should_refuse_a_plan_that_still_carries_missing_objects() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_DEPLOY);
    write(tmp.path(), "sql/routines/10_main.sql", ROUTINE_SEED_COUNT);
    write(tmp.path(), "sql/ddl/tables.sql", "-- 什么都没建\n");
    let file = abs_file(&tmp.path().join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        tmp.path(),
        "prc_seed_count",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_seed_count", "file": file},
                {"id": 2, "type": "table", "name": "t_a"},
                {"id": 3, "type": "sequence", "name": "seq_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "table_access", "modes": ["write"], "file": file, "line": 1},
                {"source": 1, "target": 3, "type": "uses_sequence", "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_FAIL, "夹具计划要带缺件: {out}");
    let plan = tmp.path().join(".spinduck/plan/prc_seed_count.toml");
    let db = tmp.path().join("work.duckdb");

    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_FAIL, "缺件计划不可部署: {out}");
    assert!(out.contains("MISS relation:t_a <- prc_seed_count"), "{out}");
    assert!(
        !db.exists(),
        "判红不得留下半成品库文件（建一半比不建更危险）"
    );
}

#[test]
fn should_refuse_to_call_a_plan_that_still_carries_missing_objects() {
    // `call --plan` 与 `deploy` 同一条前置门：缺件表非空即拒，**连内核都不开**。
    // 少了这一步，计划里写着"缺一张表"却照样进 `eng.call`，就又回到 D5 要消灭的静默形态。
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_DEPLOY);
    write(tmp.path(), "sql/routines/10_main.sql", ROUTINE_SEED_COUNT);
    write(tmp.path(), "sql/ddl/tables.sql", "-- 什么都没建\n");
    let file = abs_file(&tmp.path().join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        tmp.path(),
        "prc_seed_count",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_seed_count", "file": file},
                {"id": 2, "type": "table", "name": "t_a"},
                {"id": 3, "type": "sequence", "name": "seq_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "table_access", "modes": ["write"], "file": file, "line": 1},
                {"source": 1, "target": 3, "type": "uses_sequence", "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_FAIL, "夹具计划要带缺件: {out}");
    let plan = tmp.path().join(".spinduck/plan/prc_seed_count.toml");
    let db = tmp.path().join("work.duckdb");

    let (code, out, err) = cli(&[
        "call",
        "--plan",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
        "0",
    ]);
    assert_eq!(
        out,
        "MISS relation:t_a <- prc_seed_count\n\
         MISS sequence:seq_a <- prc_seed_count\n\
         call: target=prc_seed_count missing=2 拒绝执行（补齐缺件后重跑 plan）\n",
        "{out}"
    );
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(
        !db.exists(),
        "判红不得留下半成品库文件（建一半比不建更危险）"
    );
}

#[test]
fn should_exit_1_with_env_0010_when_the_plan_file_given_to_call_is_absent() {
    // 计划路径是 `call --plan` 的第一手输入：读不到就该立刻判红，而不是继续找项目、
    // 更不是先开一个空库再报错（空库文件是本轮最容易留下的脏产物）。
    let tmp = tempfile::tempdir().unwrap();
    let plan = tmp.path().join(".spinduck/plan/prc_none.toml");
    let db = tmp.path().join("work.duckdb");
    let (code, out, err) = cli(&[
        "call",
        "--plan",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
        "0",
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0010]"), "{out}");
    assert!(
        out.contains(".spinduck/plan/prc_none.toml"),
        "诊断要点名读不到的计划文件: {out}"
    );
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(!db.exists(), "计划没读到就不得留下空库文件");
}

#[test]
fn should_refuse_a_plan_call_when_a_routine_file_it_lists_is_absent() {
    // 计划与磁盘分叉在 `call` 侧同样致命：`deploy` 缺文件只是建不出环境，`call` 缺文件
    // 却可能"注册了一半例程"就跑——所以解析计划文件必须发生在开库之前。
    let tmp = tempfile::tempdir().unwrap();
    let plan = deployed_plan(tmp.path());
    let db = tmp.path().join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    std::fs::remove_file(tmp.path().join("sql/routines/10_main.sql")).unwrap();

    let (code, out, _) = cli(&[
        "call",
        "--plan",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
        "0",
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(
        out.contains("sql/routines/10_main.sql"),
        "诊断要点名缺失的例程文件: {out}"
    );
}

#[test]
fn should_refuse_when_a_file_listed_by_the_plan_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let plan = deployed_plan(tmp.path());
    // 计划与磁盘分叉（改名/删文件/换机器）：deploy 必须点名缺哪个文件，而不是建半个环境
    std::fs::remove_file(tmp.path().join("sql/ddl/tables.sql")).unwrap();
    let db = tmp.path().join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(
        out.contains("sql/ddl/tables.sql"),
        "诊断要点名缺失文件: {out}"
    );
    assert!(!db.exists(), "缺文件即红，不落盘半成品");
}

#[test]
fn should_refuse_a_plan_written_by_an_unknown_schema_version() {
    let tmp = tempfile::tempdir().unwrap();
    let plan = deployed_plan(tmp.path());
    let text = std::fs::read_to_string(&plan).unwrap();
    std::fs::write(
        &plan,
        text.replace("schema_version = 1", "schema_version = 2"),
    )
    .unwrap();
    let db = tmp.path().join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("schema_version=2"), "{out}");
    assert!(!db.exists());
}

#[test]
fn should_exit_2_when_deploy_has_no_db_target() {
    let tmp = tempfile::tempdir().unwrap();
    let plan = deployed_plan(tmp.path());
    let (code, out, err) = cli(&["deploy", &plan.to_string_lossy()]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.starts_with("[ENG-0010] usage: deploy"), "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

/// 考题语料的 9 个 `1_V_*.sql` 把 `/*FORCE*/` 插在 `CREATE OR REPLACE` 与 `VIEW` 中间；
/// 关键字判定若看不见这条注释，视图体一条都不下发，库里只剩 `SELECT 1` 占位视图——
/// 对象存在（缺件判定不受影响），但读该视图列的语句在这个库上必然取不到列。
#[test]
fn should_deploy_the_view_body_when_a_comment_splits_the_create_keywords() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_DEPLOY);
    write(
        tmp.path(),
        "sql/routines/10_main.sql",
        "CREATE OR REPLACE PROCEDURE prc_read_view(p_o OUT NUMBER) IS
BEGIN
  SELECT COUNT(*) INTO p_o FROM v_a WHERE id > 0;
END;
/
",
    );
    write(
        tmp.path(),
        "sql/ddl/views/1_V_A.sql",
        "CREATE OR REPLACE /*FORCE*/ VIEW v_a AS SELECT 42 AS id;\n",
    );
    let file = abs_file(&tmp.path().join("sql/routines/10_main.sql"));
    let (code, out) = plan_in_process(
        tmp.path(),
        "prc_read_view",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_read_view", "file": file},
                {"id": 2, "type": "view", "name": "v_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "table_access", "modes": ["read"], "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "夹具计划要干净: {out}");
    let plan = tmp.path().join(".spinduck/plan/prc_read_view.toml");
    let db = tmp.path().join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    // 视图体只选常量、没有基表 ⇒ 清单只有视图文件本身
    assert!(out.contains("ddl_files=1"), "{out}");

    let (code, out, _) = cli(&[
        "call",
        "--db",
        &db.to_string_lossy(),
        "--file",
        &tmp.path()
            .join("sql/routines/10_main.sql")
            .to_string_lossy(),
        "prc_read_view",
        "0",
    ]);
    assert_eq!(code, EXIT_OK, "占位视图取不到 id 列，只会红在这里: {out}");
    assert!(
        out.contains("OUT p_o = 1"),
        "视图体未下发时这一列根本不存在: {out}"
    );
}

/// 视图体的依赖也要进计划，否则"计划齐了"是假的。真题上 4 张视图因此退化成占位视图：
/// 基表一个都没下发，其中两张的基表还藏在逗号连表的第二、三位（`FROM t, u`——`FROM`
/// 只出现一次，只取第一个名字就漏掉后半边的表）。这条把两处修复一起锁在进程面：
/// 闭包展开视图体 + 逗号连表每一张表都算引用，再由视图体真正建进库里。
#[test]
fn should_deploy_a_view_body_whose_comma_joined_base_tables_only_the_plan_knows() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_DEPLOY);
    write(
        tmp.path(),
        "sql/routines/10_main.sql",
        "CREATE OR REPLACE PROCEDURE prc_read_view(p_o OUT NUMBER) IS
BEGIN
  SELECT COUNT(*) INTO p_o FROM v_a WHERE id > 0;
END;
/
",
    );
    write(
        tmp.path(),
        "sql/ddl/views/1_V_A.sql",
        "CREATE OR REPLACE /*FORCE*/ VIEW v_a (id) AS\n  SELECT t.id FROM t_a t, t_b b WHERE t.id = b.id;\n",
    );
    write(
        tmp.path(),
        "sql/ddl/tables/t_a.sql",
        "CREATE TABLE t_a (id INT);\n",
    );
    write(
        tmp.path(),
        "sql/ddl/tables/t_b.sql",
        "CREATE TABLE t_b (id INT);\n",
    );

    let file = abs_file(&tmp.path().join("sql/routines/10_main.sql"));
    let view = abs_file(&tmp.path().join("sql/ddl/views/1_V_A.sql"));
    let (code, out) = plan_in_process(
        tmp.path(),
        "prc_read_view",
        None,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_read_view", "file": file},
                {"id": 2, "type": "view", "name": "v_a"},
                {"id": 3, "type": "table", "name": "t_a"},
                {"id": 4, "type": "table", "name": "t_b"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "table_access", "modes": ["read"], "file": file, "line": 1},
                {"source": 2, "target": 3, "type": "depends_on", "file": view, "line": 1},
                {"source": 2, "target": 4, "type": "depends_on", "file": view, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "例程只读视图，两张基表靠视图体进计划: {out}");
    let plan = tmp.path().join(".spinduck/plan/prc_read_view.toml");
    let text = std::fs::read_to_string(&plan).unwrap();
    assert!(text.contains("sql/ddl/tables/t_a.sql"), "{text}");
    assert!(text.contains("sql/ddl/tables/t_b.sql"), "{text}");

    let db = tmp.path().join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &plan.to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains("ddl_files=3"),
        "两张基表 + 一个视图文件: {out}"
    );

    let (code, out, _) = cli(&[
        "call",
        "--db",
        &db.to_string_lossy(),
        "--file",
        &tmp.path()
            .join("sql/routines/10_main.sql")
            .to_string_lossy(),
        "prc_read_view",
        "0",
    ]);
    assert_eq!(code, EXIT_OK, "占位视图没有 id 列，绑定阶段就红: {out}");
    assert!(
        out.contains("OUT p_o = 0"),
        "空表数回 0 —— 视图体是真下发并执行过的: {out}"
    );
}
