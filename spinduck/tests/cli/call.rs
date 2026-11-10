//! `call`：例程注册与调用、编码判定（D9① 判红 + D9② 真解码）、以及 design §3.3 的三形态。

use super::{
    cli, purchase_file, write, write_bytes, write_flag_routine, write_gbk_routine, EXIT_FAIL,
    EXIT_OK, EXIT_USAGE, PURCHASE_DATE, PURCHASE_DDL, PURCHASE_GETDAY, PURCHASE_ID,
    PURCHASE_IF_DEAL, PURCHASE_MAIN, PURCHASE_ROUTINE, PURCHASE_SEED, P_LITERAL_UTF8, P_NULL, P_X,
};

#[test]
fn should_call_routine_from_file_and_print_out_params() {
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "p_x.sql", P_X);
    let (code, out, _) = cli(&[
        "call",
        "--file",
        &f.to_string_lossy(),
        "prc_x",
        "21",
        "null",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "OUT p_o = 42\n", "{out}");
}

#[test]
fn should_call_print_ok_line_when_routine_returns_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "p_null.sql", P_NULL);
    let (code, out, _) = cli(&["call", "--file", &f.to_string_lossy(), "p_null"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "CALL p_null OK\n", "{out}");
}

#[test]
fn should_exit_1_with_biz_0150_when_calling_unregistered_routine() {
    let (code, out, _) = cli(&["call", "pkg.missing", "1"]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(
        out, "[BIZ-0150] routine pkg.missing not registered\n",
        "{out}"
    );
}

#[test]
fn should_exit_1_with_env_0010_when_call_file_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.sql").to_string_lossy().into_owned();
    let (code, out, _) = cli(&["call", "--file", &missing, "p_x"]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(
        out.starts_with(&format!("[ENV-0010] read {missing}: ")),
        "{out}"
    );
}

#[test]
fn should_decode_a_gbk_source_and_return_the_same_literal_as_its_utf8_twin() {
    // D9②（design §3.5 / §7 A2）：执行面按 GBK 解码，**同源码仅编码不同 → 结果逐字相同**。
    // 这一条锁掉的正是 A0a 实测的 MATCH/NO_MATCH 分叉的反面：改动前 UTF-8 得 `中文`、GBK 得
    // U+FFFD 串；D9① 后 GBK 一律判红（对交付件里 3,149 个文件等于"跑不起来"）；现在两者都
    // rc=0 且输出一致。语料文件逐字不动，转码只发生在读入时。
    let tmp = tempfile::tempdir().unwrap();
    let gbk = write_gbk_routine(tmp.path(), "gbk.sql");
    let utf8 = write(tmp.path(), "utf8.sql", P_LITERAL_UTF8);
    let (code_gbk, out_gbk, err_gbk) =
        cli(&["call", "--file", &gbk.to_string_lossy(), "prc_gbk", "null"]);
    let (code_utf8, out_utf8, _) =
        cli(&["call", "--file", &utf8.to_string_lossy(), "prc_gbk", "null"]);
    assert_eq!(code_utf8, EXIT_OK, "{out_utf8}");
    assert_eq!(code_gbk, EXIT_OK, "{out_gbk}");
    assert_eq!(out_utf8, "OUT p_o = 中文\n", "{out_utf8}");
    assert_eq!(out_gbk, out_utf8, "GBK 与 UTF-8 两文件须给出同一个字面量");
    assert_eq!(err_gbk, "", "全部输出走 stdout");
}

#[test]
fn should_exit_1_with_env_0030_when_call_source_is_valid_in_neither_encoding() {
    // D9① 判红面的另一半在 D9② 之后仍然成立：两种编码都不合法的字节一律红，绝不退回有损解码。
    // 输入从"GBK 的 `中文`"换成 `0xFF 0xFE`（既非合法 UTF-8，也非合法 GBK 双字节）——
    // 前者现在是上一那条正例的输入。诊断形状仍对齐 `verify` 的 ENV-0030 + 读取路径。
    let tmp = tempfile::tempdir().unwrap();
    let broken = write_bytes(
        tmp.path(),
        "broken.sql",
        b"CREATE OR REPLACE PROCEDURE prc_gbk(p_o OUT VARCHAR2) IS BEGIN p_o := '\xff\xfe'; END;\n",
    );
    let (code, out, _) = cli(&[
        "call",
        "--file",
        &broken.to_string_lossy(),
        "prc_gbk",
        "null",
    ]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(
        out.starts_with(&format!("[ENV-0030] 读取 {}", broken.display())),
        "须对齐 verify 的 ENV-0030 形状: {out}"
    );
    assert!(out.contains("valid UTF-8"), "{out}");
    assert!(out.contains("valid GBK"), "{out}");
    assert!(!out.contains("OUT "), "判红前不得产出任何结果: {out}");
}

#[test]
fn should_return_the_same_match_for_a_gbk_and_a_utf8_source_with_one_utf8_actual() {
    // design §3.5 的判据实验（A0a / V27.4）在 D9② 后的复跑。改动前实测：
    //   GBK  源 + UTF-8 参数 → RETURN NO_MATCH rc=0   ← 静默错值，本文最警惕的形态
    //   UTF-8 源 + UTF-8 参数 → RETURN MATCH   rc=0
    // 即 `WHERE status = '已成交'` 会因为源码编码而变成永不成立的比较，且两条都绿。
    // D9① 把 GBK 那条变成 rc=1（对而不响的中间态）；D9② 要求两者同为 rc=0 且同为 MATCH。
    let tmp = tempfile::tempdir().unwrap();
    let gbk = write_flag_routine(tmp.path(), "gbk.sql", true);
    let utf8 = write_flag_routine(tmp.path(), "utf8.sql", false);
    let (code_gbk, out_gbk, _) = cli(&[
        "call",
        "--file",
        &gbk.to_string_lossy(),
        "fnc_flag",
        "'已成交'",
    ]);
    let (code_utf8, out_utf8, _) = cli(&[
        "call",
        "--file",
        &utf8.to_string_lossy(),
        "fnc_flag",
        "'已成交'",
    ]);
    assert_eq!(code_utf8, EXIT_OK, "{out_utf8}");
    assert_eq!(out_utf8, "RETURN MATCH\n", "{out_utf8}");
    assert_eq!(code_gbk, EXIT_OK, "{out_gbk}");
    assert_eq!(out_gbk, out_utf8, "同源码仅编码不同 → 结果必须相同");
}

#[test]
fn should_ignore_ddl_and_seed_files_given_to_call() {
    // 形态 B ≡ 形态 A：`Engine::register` 只编译例程声明，表/种子 DDL 既不执行也不告警
    // （`src/engine.rs:124`）。`tests/guard/guard_register_silent_drop.rs` 锁的是
    // register 层，本条锁它在真语料 + 真二进制上的端到端形态（V27.1 结论 1）。
    //
    // 期望随 D5（A2-4）同批改写：旧断言锁 `[KERNEL-0010] fnc_com_if_deal_date does not
    // exist`，那是"一路跑到内核才炸"的证据；现在红发生在内核之前，A/B/C 三形态在 CLI 面上
    // 同形。本条的意图反而更好证明——**带上 tables.sql 与 seed.sql 也照样报同一张缺表**，
    // 说明这两个文件对 `call` 彻底无效。内核面那条证据仍留在 V27.1 与守护测试里，未被删改。
    let main = purchase_file(PURCHASE_MAIN);
    let ddl = purchase_file(PURCHASE_DDL);
    let seed = purchase_file(PURCHASE_SEED);

    let (code_a, out_a, _) = cli(&[
        "call",
        "--file",
        &main,
        PURCHASE_ROUTINE,
        PURCHASE_DATE,
        PURCHASE_ID,
    ]);
    let (code_b, out_b, _) = cli(&[
        "call",
        "--file",
        &main,
        "--file",
        &ddl,
        "--file",
        &seed,
        PURCHASE_ROUTINE,
        PURCHASE_DATE,
        PURCHASE_ID,
    ]);
    assert_eq!(
        out_a,
        "MISS relation:par_sys_purchase <- fnc_get_purchase_js_days\n\
         call: target=fnc_get_purchase_js_days missing=1 未执行（design D5 缺件即红）\n",
        "{out_a}"
    );
    assert_eq!(code_a, EXIT_FAIL, "{out_a}");
    assert_eq!((code_b, out_b), (code_a, out_a), "递进建表脚本必须完全无效");
}

#[test]
fn should_list_the_missing_relation_and_execute_nothing_when_a_chained_table_is_absent() {
    // design D5（A2-4）：链路上有对象内核里没有 ⇒ 列缺件清单、rc=1，红之前**不执行**例程。
    // 本条用的是真语料的合成形态（一条链、两个缺表、一个未登记的被调过程），锁的是缺件表
    // 的**完整性与序**：对象按标识排序、去重，例程缺件与表缺件同列——`plan` 的 `[[missing]]`
    // 什么形状，这里的红就是什么形状（同一条 BFS，见 src/plan/closure.rs::walk_chain）。
    let tmp = tempfile::tempdir().unwrap();
    let f = write(
        tmp.path(),
        "gap.sql",
        "CREATE OR REPLACE PROCEDURE pkg_a.prc_gap(p_o OUT NUMBER) IS
BEGIN
  SELECT COUNT(*) INTO p_o FROM t_gap_a;
  SELECT COUNT(*) INTO p_o FROM t_gap_b;
  SELECT COUNT(*) INTO p_o FROM t_gap_a;
  prc_missing_sibling(1);
END;
/",
    );
    let (code, out, err) = cli(&["call", "--file", &f.to_string_lossy(), "pkg_a.prc_gap"]);
    assert_eq!(
        out,
        "MISS relation:t_gap_a <- pkg_a.prc_gap\n\
         MISS relation:t_gap_b <- pkg_a.prc_gap\n\
         MISS routine:prc_missing_sibling <- pkg_a.prc_gap\n\
         call: target=pkg_a.prc_gap missing=3 未执行（design D5 缺件即红）\n",
        "{out}"
    );
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(!out.contains("OUT "), "红之前不得产出任何结果: {out}");
}

#[test]
fn should_exit_1_with_the_missing_relation_when_deps_are_registered_but_tables_are_absent() {
    // 形态 C 的旧结局（V27.1 结论 2/3，本条原期望）：补齐依赖函数、仍不建表 ⇒
    // `RETURN 0` + rc=0。`0` 恰是语料自己 `WHEN OTHERS` 归的合法值，rc=0 时没有任何
    // 东西能把它和一次正确复现区分开——这就是 design D5 要消灭的静默形态。
    // 期望值随行为一起改（用户 2026-11-10 授权），旧断言 `RETURN 0`/rc=0 一并作废。
    let main = purchase_file(PURCHASE_MAIN);
    let getday = purchase_file(PURCHASE_GETDAY);
    let if_deal = purchase_file(PURCHASE_IF_DEAL);
    let (code, out, _) = cli(&[
        "call",
        "--file",
        &main,
        "--file",
        &getday,
        "--file",
        &if_deal,
        PURCHASE_ROUTINE,
        PURCHASE_DATE,
        PURCHASE_ID,
    ]);
    assert_eq!(
        out,
        "MISS relation:par_sys_purchase <- fnc_get_purchase_js_days\n\
         call: target=fnc_get_purchase_js_days missing=1 未执行（design D5 缺件即红）\n",
        "{out}"
    );
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(!out.contains("RETURN"), "红之前不许产出 RETURN: {out}");
}

#[test]
fn should_gate_an_expression_called_function_and_its_own_table_before_executing() {
    // A2-5：`v := fnc_hidden()` 这类**表达式级**裸名调用过去不进闭包，于是被调函数
    // 它自己的表对门不可见——运行期照样调它（分派命中例程目录），在内核里撞到
    // "Table does not exist"，报错的是内核而不是缺件门。现在同一条 BFS 收它，
    // 缺件归属记在被调函数名下（与 `plan` 的 `[[missing]]` 逐字同形）。
    let tmp = tempfile::tempdir().unwrap();
    let f = write(
        tmp.path(),
        "expr_gap.sql",
        "CREATE OR REPLACE FUNCTION fnc_hidden RETURN INT IS
  v INT;
BEGIN
  SELECT COUNT(*) INTO v FROM t_hidden;
  RETURN v;
END;
/

CREATE OR REPLACE PROCEDURE pkg_a.prc_outer(p_o OUT NUMBER) IS
  v INT;
BEGIN
  v := fnc_hidden();
  p_o := v;
END;
/",
    );
    let (code, out, err) = cli(&[
        "call",
        "--file",
        &f.to_string_lossy(),
        "pkg_a.prc_outer",
        "0",
    ]);
    assert_eq!(
        out,
        "MISS relation:t_hidden <- fnc_hidden\n\
         call: target=pkg_a.prc_outer missing=1 未执行（design D5 缺件即红）\n",
        "{out}"
    );
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
    assert!(!out.contains("OUT "), "红之前不得产出任何结果: {out}");
    assert!(
        !out.contains("KERNEL-0010"),
        "缺件门要拦在撞内核之前: {out}"
    );
}

// ------------------------------------------------- 执行面开关接线（V26.10 / 复核应修 C）

/// 语料同款 LOOP/COMMIT/EXIT 例程：`COMMIT` 走 `--batch-commit` 的吞并路径。
const LOOP_COMMIT_SQL: &str = "CREATE OR REPLACE PROCEDURE prc_fill(p_n IN INT) IS\n  v_i INT := 0;\nBEGIN\n  LOOP\n    v_i := v_i + 1;\n    COMMIT;\n    EXIT WHEN v_i >= p_n;\n  END LOOP;\nEND;\n/\n";

#[test]
fn should_call_routine_with_batch_commit_and_step_budget_flags() {
    // 开关的实际接线此前只有解析层测试面：这里驱动**打包后的二进制**走完
    // parse → cli::run → Engine::call，证明 `eng.batch_commit` / `eng.step_budget` 真被赋值。
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "routines.sql", LOOP_COMMIT_SQL);
    let (code, out, err) = cli(&[
        "call",
        "--file",
        &f.to_string_lossy(),
        "--batch-commit",
        "--step-budget",
        "1000000",
        "prc_fill",
        "5",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "CALL prc_fill OK\n", "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_reject_step_budget_zero_with_usage_exit() {
    // 0 拒绝在解析层：usage 退出码 2 + 消息含 flag 名
    let (code, out, err) = cli(&["call", "--step-budget", "0", "prc_x"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("--step-budget"), "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_call_routine_on_default_off_path() {
    // 与 ON 路径同例程：锁「开关缺省不改变契约」
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "routines.sql", LOOP_COMMIT_SQL);
    let (code, out, err) = cli(&["call", "--file", &f.to_string_lossy(), "prc_fill", "5"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "CALL prc_fill OK\n", "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
}
