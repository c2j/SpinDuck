//! `query`：跑一条 SQL 落盘结果快照（design §6.2 / §7 A5 本地半边）。
//!
//! 本层证明的三件事都只能在真子进程里看：快照文件真的落盘且字节可预期、判红时不留
//! 脏产物（`--out` 一个字节都不写）、以及语句文本进内核前只被削掉一个结尾分号。
//! 归一与逐值裁决不在这里测——那是 `snapshot` 单测与 `compare` 契约的事。

use super::{
    abs_file, cli, compile_project_in_process, cw_graph, write, EXIT_FAIL, EXIT_OK, EXIT_USAGE,
};

const PROJECT_QUERY: &str =
    "[project]\nname = \"q\"\nkernel = \"1.5.5\"\n\n[paths]\nddl = \"sql/ddl\"\nroutines = \"sql/routines\"\nhome = \".spinduck\"\n";

const DDL_T_A: &str = "CREATE TABLE t_a (id INT);\n";

/// 一次调用插一行，跑两次即库里两行——快照行数由此可控。
const ROUTINE_INSERT: &str = "CREATE OR REPLACE PROCEDURE prc_ins(p_i IN NUMBER) IS
BEGIN
  INSERT INTO t_a VALUES (p_i);
END;
/
";

/// 部署一个带 `t_a(id INT)` 的落盘库并塞进 `1`、`2` 两行，返回库文件路径。
///
/// `compare` 的端到端用例也用它（`query` → `compare` 是同一条链的两半，夹具不必再写一遍）。
pub(crate) fn seeded_db(dir: &std::path::Path) -> std::path::PathBuf {
    write(dir, "spinduck.toml", PROJECT_QUERY);
    write(dir, "sql/routines/10_ins.sql", ROUTINE_INSERT);
    write(dir, "sql/ddl/tables.sql", DDL_T_A);
    let file = abs_file(&dir.join("sql/routines/10_ins.sql"));
    let (code, out) = compile_project_in_process(
        dir,
        &["prc_ins"],
        None,
        false,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "procedure", "name": "prc_ins", "file": file},
                {"id": 2, "type": "table", "name": "t_a"}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "table_access", "modes": ["write"], "file": file, "line": 1}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "夹具计划要干净: {out}");
    let db = dir.join("work.duckdb");
    let (code, out, _) = cli(&[
        "deploy",
        &dir.join(".spinduck/plan/prc_ins.toml").to_string_lossy(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    for v in ["1", "2"] {
        let (code, out, _) = cli(&[
            "call",
            "--db",
            &db.to_string_lossy(),
            "--file",
            &dir.join("sql/routines/10_ins.sql").to_string_lossy(),
            "prc_ins",
            v,
        ]);
        assert_eq!(code, EXIT_OK, "种子行 {v} 未插入: {out}");
    }
    db
}

#[test]
fn should_dump_the_result_set_of_one_sql_file_as_a_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path());
    // 文件末尾的分号是书写习惯：内核 prepare 不认它，削掉是 query 的契约
    write(
        tmp.path(),
        "sql/count.sql",
        "SELECT COUNT(*) AS n FROM t_a;\n",
    );
    let out_file = tmp.path().join("snap.json");

    let (code, out, err) = cli(&[
        "query",
        "--db",
        &db.to_string_lossy(),
        "--sql-file",
        &tmp.path().join("sql/count.sql").to_string_lossy(),
        "--out",
        &out_file.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert_eq!(
        out,
        format!("query: rows=1 cols=1 out={}\n", out_file.to_string_lossy())
    );
    // 一行一行的排布是契约的一部分：快照本身要能 diff、能在评审里直接读；
    // 结尾换行也在契约内（否则 cat 把产物和下一条提示符黏在一起，diff 多报一行）
    assert_eq!(
        std::fs::read_to_string(&out_file).unwrap(),
        r#"{
  "schema": ["n"],
  "rows": [
    ["2"]
  ]
}
"#
    );
}

#[test]
fn should_snapshot_an_empty_result_set_as_zero_rows() {
    // 空结果集是"绿"的一种形态，`rows` 必须是空数组而不是缺件
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path());
    write(
        tmp.path(),
        "sql/none.sql",
        "SELECT id FROM t_a WHERE id > 1000000",
    );
    let out_file = tmp.path().join("empty.json");
    let (code, out, _) = cli(&[
        "query",
        "--db",
        &db.to_string_lossy(),
        "--sql-file",
        &tmp.path().join("sql/none.sql").to_string_lossy(),
        "--out",
        &out_file.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(
        std::fs::read_to_string(&out_file).unwrap(),
        r#"{
  "schema": ["id"],
  "rows": []
}
"#
    );
}

#[test]
fn should_leave_no_snapshot_file_when_the_statement_fails() {
    // 缺表判红本来就是 §3.3 形态 C 的形态；这里额外锁"红的时候不许留下半个产物"——
    // 一份空 JSON 或半份快照会被下一步 compare 当成"两边都空 ⇒ 一致"
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path());
    write(tmp.path(), "sql/bad.sql", "SELECT * FROM t_missing");
    let out_file = tmp.path().join("bad.json");
    let (code, out, err) = cli(&[
        "query",
        "--db",
        &db.to_string_lossy(),
        "--sql-file",
        &tmp.path().join("sql/bad.sql").to_string_lossy(),
        "--out",
        &out_file.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[KERNEL-0010]"), "{out}");
    assert!(out.contains("t_missing"), "诊断要点名缺的表: {out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(
        !out_file.exists(),
        "语句失败就不得留下快照文件（半份快照会被 compare 判成一致）"
    );
}

#[test]
fn should_reject_a_file_that_holds_more_than_one_statement() {
    // 契约是"一条 SQL"。内核这条路实测不可信：DuckDB 把两句都执行、只回最后一句的结果，
    // 于是静默得到一份"少了一句"的快照——所以切句判定在 core 做，且在任何执行之前。
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path());
    write(tmp.path(), "sql/two.sql", "SELECT 1; SELECT 2");
    let out_file = tmp.path().join("two.json");
    let (code, out, _) = cli(&[
        "query",
        "--db",
        &db.to_string_lossy(),
        "--sql-file",
        &tmp.path().join("sql/two.sql").to_string_lossy(),
        "--out",
        &out_file.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0031]"), "{out}");
    assert!(out.contains("2 条语句"), "诊断要说清有几条: {out}");
    assert!(!out_file.exists(), "{out}");
}

#[test]
fn should_read_the_sql_file_before_opening_the_database() {
    // 读不到 SQL 文件时连库都不开：DuckDB 的 open 即建文件，晚一步就留下一个空库
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("brand_new.duckdb");
    let out_file = tmp.path().join("snap.json");
    let (code, out, err) = cli(&[
        "query",
        "--db",
        &db.to_string_lossy(),
        "--sql-file",
        &tmp.path().join("sql/absent.sql").to_string_lossy(),
        "--out",
        &out_file.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0010]"), "{out}");
    assert!(out.contains("absent.sql"), "诊断要点名读不到的文件: {out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(!db.exists(), "SQL 文件没读到就不得开出库文件");
    assert!(!out_file.exists());
}

#[test]
fn should_refuse_a_gbk_sql_file_because_the_statement_must_be_decoded() {
    // D9② 的口径延伸到 query：GBK 语料是交付件现实，能严格解码就照跑，
    // 字面量必须逐字还原（不能像 D9① 之前那样有损解码后静默变形）
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path());
    let gbk = encoding_rs::GBK.encode("中文").0;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"SELECT '");
    bytes.extend_from_slice(gbk.as_ref());
    bytes.extend_from_slice(b"' AS v");
    let sql = super::write_bytes(tmp.path(), "sql/gbk.sql", &bytes);
    let out_file = tmp.path().join("gbk.json");
    let (code, out, _) = cli(&[
        "query",
        "--db",
        &db.to_string_lossy(),
        "--sql-file",
        &sql.to_string_lossy(),
        "--out",
        &out_file.to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        std::fs::read_to_string(&out_file).unwrap().contains("中文"),
        "GBK 字面量未逐字还原: {out}"
    );
}

#[test]
fn should_exit_2_when_query_is_missing_the_out_or_sql_file_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path());
    let (code, out, _) = cli(&["query", "--db", &db.to_string_lossy()]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.starts_with("[ENG-0010] usage: query"), "{out}");
    assert!(out.contains("--sql-file"), "缺件面要指认缺哪个旗标: {out}");

    let (code, out, _) = cli(&[
        "query",
        "--db",
        &db.to_string_lossy(),
        "--sql-file",
        &tmp.path().join("sql/x.sql").to_string_lossy(),
    ]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("--out"), "{out}");

    let (code, out, _) = cli(&["query"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("--db"), "缺件面要指认 --db: {out}");
}

#[test]
fn should_exit_2_when_a_positional_is_given() {
    // F2-A：query 三件输入全旗标，不再收位置参数（旧 <db> 位置形态一并拒绝）
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path());
    let (code, out, _) = cli(&[
        "query",
        &db.to_string_lossy(),
        "--sql-file",
        "s.sql",
        "--out",
        "o.json",
    ]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("不接受位置参数"), "{out}");
}
