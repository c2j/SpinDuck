//! `call --trace` 契约（§11.1/§11.2）：审计日志与 sidecar 落盘、stdout 契约不变、
//! 缺件即红时不产出任何日志文件。

use super::*;
use std::path::Path;

/// 主角：一条 SQL（`SELECT 1`）+ 一次赋值，日志里至少有一条 SQL_EXEC。
const P_SQL: &str = "CREATE OR REPLACE PROCEDURE prc_probe(p_i IN NUMBER, p_o OUT NUMBER) IS \
                     v NUMBER; \
                     BEGIN \
                       SELECT 1 INTO v; \
                       p_o := p_i * 2; \
                     END;";

/// 缺件主角：引用的表在内核里不存在 → D5 门拒绝执行。
const P_GAP: &str = "CREATE OR REPLACE PROCEDURE prc_gap IS \
                     BEGIN \
                       INSERT INTO t_missing VALUES (1); \
                     END;";

/// 对齐主角：一条跑两次、一条走不到的分支、一条动态语句（文本只存在于运行期）。
const P_ALIGN: &str = "CREATE OR REPLACE PROCEDURE prc_align(p_i IN NUMBER, p_o OUT NUMBER) IS \
                       v NUMBER; \
                       BEGIN \
                         SELECT 1 INTO v; \
                         SELECT 1 INTO v; \
                         IF p_i = 1 THEN \
                           SELECT 2 INTO v; \
                         END IF; \
                         EXECUTE IMMEDIATE 'SELECT 42' INTO v; \
                         p_o := v; \
                       END;";

fn trace_line(line: &str) -> bool {
    line.starts_with("[trace] ")
}

/// 取出 text 行里的 sqlid（第 5 个空格段：`[trace] ts seq EVENT sq-xxxx …`）。
fn sql_id_of_text_line(line: &str) -> Option<String> {
    let mut it = line.split(' ').skip_while(|t| !t.starts_with("sq-"));
    it.next().map(|s| s.to_string())
}

#[test]
fn should_write_a_text_audit_log_and_a_sql_sidecar_for_a_traced_call() {
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "pkg.sql", P_SQL);
    let log = dir.path().join("run.log");
    let (code, out, err) = cli(&[
        "call",
        "--file",
        &src.to_string_lossy(),
        "--trace",
        &log.to_string_lossy(),
        "prc_probe",
        "21",
        "null",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert_eq!(out, "OUT p_o = 42\n", "--trace 不得改 stdout 契约");
    let text = std::fs::read_to_string(&log).expect("审计日志必须落盘");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "默认 bind 级只记 SQL_EXEC（IR_STMT 属 all 级）: {lines:?}"
    );
    assert!(trace_line(lines[0]), "{}", lines[0]);
    assert!(lines[0].contains("SQL_EXEC"), "{}", lines[0]);
    let id = sql_id_of_text_line(lines[0]).expect("行内必须有 sqlid");
    assert_eq!(id.len(), 15, "sq- + 12 位十六进制: {id}");
    // sidecar：sqlid → 全文，一行一条，grep 反查
    let side = std::fs::read_to_string(format!("{}.sql", log.display()))
        .expect("sidecar 必须与日志同前缀落盘");
    assert_eq!(side.lines().count(), 1, "{side}");
    let (got_id, got_sql) = side
        .split_once('\t')
        .expect("sidecar 行形态 = sqlid<TAB>全文");
    assert_eq!(got_id, id);
    assert!(
        got_sql.contains("SELECT 1"),
        "sidecar 存的是可反查的全文: {got_sql}"
    );
}

#[test]
fn should_write_jsonl_lines_with_the_contract_header_when_format_is_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "pkg.sql", P_SQL);
    let log = dir.path().join("run.jsonl");
    let (code, _, _) = cli(&[
        "call",
        "--file",
        &src.to_string_lossy(),
        "--trace",
        &log.to_string_lossy(),
        "--trace-format",
        "jsonl",
        "prc_probe",
        "21",
        "null",
    ]);
    assert_eq!(code, EXIT_OK);
    let text = std::fs::read_to_string(&log).expect("审计日志必须落盘");
    let line = text.lines().next().expect("至少一行");
    let v: serde_json::Value = serde_json::from_str(line).expect("每行一条 JSON");
    assert_eq!(v["ch"], "trace");
    assert_eq!(v["lvl"], "INFO");
    assert_eq!(v["event"], "SQL_EXEC");
    assert_eq!(v["seq"], 1);
    assert_eq!(v["proc"], "prc_probe");
    assert!(v["sid"].is_string(), "会话标识必须来自运行环境");
    assert_eq!(v["cid"], "c-000001");
    assert!(v["ts"].as_str().unwrap_or_default().ends_with("Z"));
    let id = v["detail"]["sql_id"].as_str().expect("sql_id 入 detail");
    let side = std::fs::read_to_string(format!("{}.sql", log.display())).unwrap();
    assert!(side.starts_with(id), "sidecar 与日志行同一 sqlid: {side}");
}

#[test]
fn should_record_ir_stmt_lines_only_at_trace_level_all() {
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "pkg.sql", P_SQL);
    let log = dir.path().join("all.log");
    let (code, _, _) = cli(&[
        "call",
        "--file",
        &src.to_string_lossy(),
        "--trace",
        &log.to_string_lossy(),
        "--trace-level",
        "all",
        "prc_probe",
        "21",
        "null",
    ]);
    assert_eq!(code, EXIT_OK);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.lines().any(|l| l.contains("IR_STMT")),
        "all 级必须逐条记 IR 指令:\n{text}"
    );
    assert!(text.lines().any(|l| l.contains("SQL_EXEC")));
}

#[test]
fn should_not_create_the_trace_file_when_the_call_refuses_for_missing_relations() {
    // 缺件即红 = 一条都没执行；日志与 sidecar 都不该出现"跑过什么"的假象
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "pkg.sql", P_GAP);
    let log = dir.path().join("gap.log");
    let (code, out, _) = cli(&[
        "call",
        "--file",
        &src.to_string_lossy(),
        "--trace",
        &log.to_string_lossy(),
        "prc_gap",
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("缺件即红"), "{out}");
    assert!(!Path::new(&log).exists(), "拒绝执行时不得留下日志文件");
    assert!(
        !Path::new(&format!("{}.sql", log.display())).exists(),
        "拒绝执行时不得留下 sidecar"
    );
}

#[test]
fn should_exit_usage_when_the_trace_level_is_not_a_known_name() {
    let (code, out, _) = cli(&["call", "--trace", "x.log", "--trace-level", "loud", "prc_x"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("[ENG-0010]"), "{out}");
    assert!(out.contains("--trace-level <stmt|sql|bind|all>"), "{out}");
}

#[test]
fn should_refuse_to_overwrite_an_existing_trace_log() {
    // 审计语义：重跑等于清掉上一轮"跑过什么"的证据——拒绝，且拒绝在执行之前。
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "pkg.sql", P_SQL);
    let log = dir.path().join("run.log");
    let prior = "上一轮的审计，不能被静默清掉\n";
    std::fs::write(&log, prior).unwrap();
    let (code, out, err) = cli(&[
        "call",
        "--file",
        &src.to_string_lossy(),
        "--trace",
        &log.to_string_lossy(),
        "prc_probe",
        "21",
        "null",
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0016]"), "既有文件必须指认出来: {out}");
    assert!(out.contains("拒绝覆盖"), "要说清为什么不跑: {out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(
        !out.contains("OUT p_o"),
        "拒绝先于执行，不能跑完再报错: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        prior,
        "既有审计日志逐字保留"
    );
    assert!(
        !Path::new(&format!("{}.sql", log.display())).exists(),
        "拒绝时不得写 sidecar"
    );
}

#[test]
fn should_annotate_explain_ir_lines_with_hits_from_the_trace_log() {
    // 关联形态 A：explain 的静态 SQL 与日志行同一指纹，逐行给出命中次数与绑定跨度。
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "pkg.sql", P_ALIGN);
    let log = dir.path().join("align.log");
    let (code, call_out, _) = cli(&[
        "call",
        "--file",
        &src.to_string_lossy(),
        "--trace",
        &log.to_string_lossy(),
        "prc_align",
        "0",
        "null",
    ]);
    assert_eq!(code, EXIT_OK, "{call_out}");
    let (code, out, err) = cli(&[
        "explain",
        "--file",
        &src.to_string_lossy(),
        "--trace",
        &log.to_string_lossy(),
        "prc_align",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(out.contains("\t[trace ×2]"), "跑过两次的静态 SQL:\n{out}");
    assert!(out.contains("\t[trace ×0]"), "分支没走到的静态 SQL:\n{out}");
    let dynamic: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("execute_dynamic"))
        .collect();
    assert_eq!(dynamic.len(), 1, "{out}");
    assert!(
        dynamic[0].contains("[trace 动态"),
        "动态语句的文本不在 IR 里:\n{out}"
    );
    assert!(out.contains("未对齐 1 条"), "{out}");
    assert!(out.contains("SELECT 42"), "报警要能认脸:\n{out}");
}

#[test]
fn should_keep_explain_output_unchanged_without_a_trace_log() {
    // 默认面逐字不变：注记只在给出日志时出现（explain 的既有契约含黄金比对）
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "pkg.sql", P_ALIGN);
    let (code, out, _) = cli(&["explain", "--file", &src.to_string_lossy(), "prc_align"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(!out.contains("[trace"), "{out}");
}
