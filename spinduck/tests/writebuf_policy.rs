//! V3-6 写缓冲策略守护（设计 §3.6 / §5.1-8）：`--batch-commit` 下
//! `IrStmt::SqlExec` 的编译期 `BufferPolicy` 必须把 QFII `INSERT dat_fund_cjqs`
//! 与 `c_fxj` 内写失败路径锁在即时执行，PL 异常处理器（`inst_log` / `CLOSE` /
//! `GOTO`）不得被 `WriteBuffer` 合批重放吞掉。结构镜像
//! `PKG_SPLIT_TRADE_STEP3_SH.sql` 的 QFII 段与 `c_fxj` 段。

#![cfg(feature = "native")]

use spinduck_core::exec::BindMap;
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::ir::{BufferPolicy, IrStmt};
use spinduck_core::{DuckExecutor, Engine, RuleRegistry, SpinValue};

const INST_LOG: &str = "CREATE OR REPLACE PROCEDURE inst_log(p_code VARCHAR2, p_msg VARCHAR2) IS \
    BEGIN INSERT INTO proc_log (code, msg) VALUES (p_code, p_msg); END;";

/// QFII 段微缩镜像：外层 FOR 游标逐行 → 三次 SELECT INTO（各自异常处理器写
/// `split_err_msg` + `inst_log` + `UPDATE` + `COMMIT` + `GOTO next_row`）→
/// `INSERT dat_fund_cjqs` 与成功 `UPDATE '成功'` 同处外层 try，处理器只调
/// `inst_log`。
const QFII_PROC: &str = "CREATE OR REPLACE PROCEDURE prc_m1_qfii(p_o_succeed OUT VARCHAR2) IS \
    v_fund_code VARCHAR2; v_x1 VARCHAR2; v_seat VARCHAR2; v_split_err_msg VARCHAR2; \
    BEGIN \
    p_o_succeed := '0'; \
    FOR res IN (SELECT seq, gddm, zqdm, req FROM qfqscjhbrbb_temp ORDER BY seq) LOOP \
    v_split_err_msg := ''; \
    BEGIN \
    SELECT t.val INTO v_fund_code FROM lk_partner t WHERE t.k = res.gddm; \
    EXCEPTION WHEN OTHERS THEN \
    v_split_err_msg := 'E1 partner ' || res.gddm; \
    inst_log('280', v_split_err_msg); \
    UPDATE qfqscjhbrbb_temp SET split_err_msg = v_split_err_msg WHERE seq = res.seq; \
    COMMIT; GOTO next_row; END; \
    BEGIN \
    SELECT t.val INTO v_x1 FROM lk_seat t WHERE t.k = res.gddm; \
    EXCEPTION WHEN OTHERS THEN \
    v_split_err_msg := 'E2 seat ' || res.gddm; \
    inst_log('280', v_split_err_msg); \
    UPDATE qfqscjhbrbb_temp SET split_err_msg = v_split_err_msg WHERE seq = res.seq; \
    COMMIT; GOTO next_row; END; \
    BEGIN \
    SELECT t.val INTO v_seat FROM lk_acct t WHERE t.k = res.gddm; \
    EXCEPTION WHEN OTHERS THEN \
    v_split_err_msg := 'E3 acct ' || res.gddm; \
    inst_log('280', v_split_err_msg); \
    UPDATE qfqscjhbrbb_temp SET split_err_msg = v_split_err_msg WHERE seq = res.seq; \
    COMMIT; GOTO next_row; END; \
    BEGIN \
    INSERT INTO dat_fund_cjqs (fund_code, req) VALUES (v_seat, res.req); \
    UPDATE qfqscjhbrbb_temp SET split_err_msg = '成功', split_flag = '0' WHERE seq = res.seq; \
    COMMIT; \
    EXCEPTION WHEN OTHERS THEN \
    inst_log('280', 'cjqs insert failed ' || res.gddm); END; \
    <<next_row>> NULL; \
    END LOOP; RETURN; END;";

/// `c_fxj` 段微缩镜像：外层静态游标 `c_v`（`<<error_row2>>` 在其 FETCH 前），
/// 动态 `OPEN c_fxj FOR`，内层 FETCH 循环里 `INSERT` 失败 → `CLOSE c_fxj` +
/// `inst_log` + `GOTO error_row2`。
const FXJ_PROC: &str = "CREATE OR REPLACE PROCEDURE prc_m1_fxj(p_o_succeed OUT VARCHAR2) IS \
    CURSOR c_v IS SELECT vname FROM ventures; \
    TYPE fxj_cur IS REF CURSOR; \
    c_fxj fxj_cur; \
    v_sql_txt VARCHAR2; v_venture VARCHAR2; v_zqdm VARCHAR2; v_req VARCHAR; \
    BEGIN \
    p_o_succeed := '0'; \
    OPEN c_v; \
    LOOP \
    <<error_row2>> \
    FETCH c_v INTO v_venture; \
    EXIT WHEN c_v%NOTFOUND; \
    v_sql_txt := 'SELECT zqdm, req FROM fxj_src ORDER BY seq'; \
    BEGIN OPEN c_fxj FOR v_sql_txt; \
    EXCEPTION WHEN OTHERS THEN inst_log('280', 'fxj open failed'); GOTO error_row2; END; \
    LOOP \
    FETCH c_fxj INTO v_zqdm, v_req; \
    EXIT WHEN c_fxj%NOTFOUND; \
    BEGIN \
    INSERT INTO dat_fund_cjqs (fund_code, req) VALUES ('FXJ', v_req); \
    COMMIT; \
    EXCEPTION WHEN OTHERS THEN \
    CLOSE c_fxj; \
    inst_log('280', 'fxj write err ' || v_zqdm); \
    GOTO error_row2; END; \
    END LOOP; \
    CLOSE c_fxj; \
    END LOOP; \
    CLOSE c_v; \
    UPDATE fxj_audit SET note = 'done' WHERE id = 1; \
    RETURN; END;";

/// 对照组：无游标、无 try、无跨表写的纯 `INSERT ... VALUES`——唯一允许标
/// `Buffered` 的形态。
const SAFE_PROC: &str = "CREATE OR REPLACE PROCEDURE prc_m1_safe IS \
    BEGIN \
    INSERT INTO audit_t (note) VALUES ('a1'); \
    INSERT INTO audit_t (note) VALUES ('a2'); \
    RETURN; END;";

/// 红线 1 收窄守护：`INSERT a; DELETE FROM b; INSERT b`——DELETE 执行时
/// b 尚无缓冲行（`flush_if_touches` 不刷 a），a 的 INSERT 是跨表写耦合
/// 必须 `Immediate`；b 的 INSERT 向前扫描看不到后续耦合，可 `Buffered`。
const DML_ORDER_PROC: &str = "CREATE OR REPLACE PROCEDURE prc_m1_dml_order IS \
    BEGIN \
    INSERT INTO audit_t (note) VALUES ('a1'); \
    DELETE FROM proc_log WHERE code = 'x'; \
    INSERT INTO proc_log (code, msg) VALUES ('k', 'v'); \
    RETURN; END;";

fn setup_engine() -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.batch_commit = true;
    let ddl = [
        "CREATE TABLE qfqscjhbrbb_temp(seq INT, gddm VARCHAR, zqdm VARCHAR, req VARCHAR, \
         split_err_msg VARCHAR, split_flag VARCHAR)",
        "CREATE TABLE lk_partner(k VARCHAR, val VARCHAR)",
        "CREATE TABLE lk_seat(k VARCHAR, val VARCHAR)",
        "CREATE TABLE lk_acct(k VARCHAR, val VARCHAR)",
        "CREATE TABLE dat_fund_cjqs(fund_code VARCHAR, req VARCHAR NOT NULL)",
        "CREATE TABLE proc_log(code VARCHAR, msg VARCHAR)",
        "CREATE TABLE fxj_src(seq INT, zqdm VARCHAR, req VARCHAR)",
        "CREATE TABLE ventures(vname VARCHAR)",
        "CREATE TABLE fxj_audit(id INT, note VARCHAR)",
        "CREATE TABLE audit_t(note VARCHAR)",
        // 五行覆盖：INSERT 失败行放最前（内核域错误进入处理器前
        // reset_session 会 ROLLBACK 本事务，此前写入不保留——引擎既有
        // 语义，与 Off/BatchAtomic 一致）；其后依次为 SELECT1/2/3 失败
        // 与成功行。
        "INSERT INTO qfqscjhbrbb_temp VALUES \
         (1,'G3','Z1',NULL,'INIT','1'),(2,'MISS1','Z1',NULL,'INIT','1'),\
         (3,'G1','Z1','ok','INIT','1'),(4,'G2','Z1','ok','INIT','1'),\
         (5,'G4','Z1','ok4','INIT','1')",
        "INSERT INTO lk_partner VALUES ('G1','FP1'),('G2','FP2'),('G3','FP3'),('G4','FP4')",
        "INSERT INTO lk_seat VALUES ('G2','FS2'),('G3','FS3'),('G4','FS4')",
        "INSERT INTO lk_acct VALUES ('G3','FA3'),('G4','FA4')",
        "INSERT INTO fxj_src VALUES (1,'ZX1',NULL),(2,'ZX2','ok')",
        "INSERT INTO ventures VALUES ('V1')",
        "INSERT INTO fxj_audit VALUES (1,'pending')",
    ];
    for sql in ddl {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(INST_LOG).unwrap();
    eng.register(QFII_PROC).unwrap();
    eng.register(FXJ_PROC).unwrap();
    eng.register(SAFE_PROC).unwrap();
    eng
}

fn query_rows(eng: &Engine, sql: &str) -> Vec<Vec<SpinValue>> {
    eng.ex.query(sql, &BindMap::new()).unwrap().fetch_many(100)
}

fn s(v: &str) -> SpinValue {
    SpinValue::Str(v.to_string())
}

/// 编译期策略断言（与运行模式无关，`register` 时即定型）：
/// `prc_m1_qfii` / `prc_m1_fxj` 的每条 `SqlExec` 必须 `Immediate`
/// （QFII INSERT 有跨表 UPDATE + 非空处理器，c_fxj INSERT 外层处理器
/// 含 CLOSE/GOTO——两条红线都命中）；`prc_m1_safe` 的两条纯
/// `INSERT ... VALUES` 是唯一允许 `Buffered` 的指令。
fn assert_buffer_policies(eng: &Engine) {
    for name in ["prc_m1_qfii", "prc_m1_fxj"] {
        let prog = &eng.catalog.lookup(name).unwrap().prog;
        for st in &prog.code {
            if let IrStmt::SqlExec {
                sql, buffer_policy, ..
            } = st
            {
                assert_eq!(
                    *buffer_policy,
                    BufferPolicy::Immediate,
                    "{name} 的 SqlExec 必须 Immediate: {sql}"
                );
            }
        }
    }
    let safe = &eng.catalog.lookup("prc_m1_safe").unwrap().prog;
    let buffered: Vec<&str> = safe
        .code
        .iter()
        .filter_map(|st| match st {
            IrStmt::SqlExec {
                sql,
                buffer_policy: BufferPolicy::Buffered,
                ..
            } => Some(sql.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        buffered.len(),
        2,
        "prc_m1_safe 应有两条 Buffered: {buffered:?}"
    );
    assert!(
        buffered.iter().all(|sql| sql.contains("audit_t")),
        "Buffered 只允许纯 INSERT ... VALUES: {buffered:?}"
    );
}

fn run_qfii_and_assert(mode: SetOrientedMode) {
    let mut eng = setup_engine();
    eng.set_oriented = mode;
    let result = eng.call("prc_m1_qfii", &[SpinValue::Null]).unwrap();
    assert_eq!(result.outs.get("p_o_succeed"), Some(&s("0")));

    // 行 1 INSERT 失败（req NULL）：split_err_msg/split_flag 保持原值，
    // 只落 inst_log；行 2/3/4 三条 SELECT 失败路径各自写入对应
    // split_err_msg 后 GOTO next_row，走不到 INSERT；行 5 成功：
    // UPDATE '成功' + split_flag='0'，cjqs 有对应行。
    let rows = query_rows(
        &eng,
        "SELECT seq, split_err_msg, split_flag FROM qfqscjhbrbb_temp ORDER BY seq",
    );
    let expected = vec![
        vec![SpinValue::Int(1), s("INIT"), s("1")],
        vec![SpinValue::Int(2), s("E1 partner MISS1"), s("1")],
        vec![SpinValue::Int(3), s("E2 seat G1"), s("1")],
        vec![SpinValue::Int(4), s("E3 acct G2"), s("1")],
        vec![SpinValue::Int(5), s("成功"), s("0")],
    ];
    assert_eq!(rows, expected, "mode={mode:?} qfqscjhbrbb_temp 终态");

    // dat_fund_cjqs 只有成功行一条（fund_code 来自 lk_acct.val='FA4'）。
    let cjqs = query_rows(&eng, "SELECT fund_code, req FROM dat_fund_cjqs");
    assert_eq!(cjqs, vec![vec![s("FA4"), s("ok4")]], "mode={mode:?} cjqs");

    // proc_log：三条 SELECT 失败 + 一条 INSERT 失败，各一次 inst_log。
    let logs = query_rows(&eng, "SELECT msg FROM proc_log ORDER BY msg");
    let msgs: Vec<String> = logs
        .iter()
        .map(|r| match &r[0] {
            SpinValue::Str(m) => m.clone(),
            other => panic!("unexpected log value {other:?}"),
        })
        .collect();
    assert_eq!(
        msgs,
        vec![
            "E1 partner MISS1".to_string(),
            "E2 seat G1".to_string(),
            "E3 acct G2".to_string(),
            "cjqs insert failed G3".to_string(),
        ],
        "mode={mode:?} proc_log"
    );
}

fn run_fxj_and_assert(mode: SetOrientedMode) {
    let mut eng = setup_engine();
    eng.set_oriented = mode;
    let result = eng.call("prc_m1_fxj", &[SpinValue::Null]).unwrap();
    assert_eq!(result.outs.get("p_o_succeed"), Some(&s("0")));

    // 第 1 行写失败：CLOSE c_fxj + inst_log + GOTO error_row2 生效，
    // 第 2 行（req='ok'）不再尝试——cjqs 为空、proc_log 只有一条。
    assert!(
        query_rows(&eng, "SELECT fund_code FROM dat_fund_cjqs").is_empty(),
        "mode={mode:?} cjqs 应为空"
    );
    let logs = query_rows(&eng, "SELECT msg FROM proc_log");
    assert_eq!(
        logs,
        vec![vec![s("fxj write err ZX1")]],
        "mode={mode:?} proc_log"
    );
    // 外层循环走完、UPDATE audit 正常路径仍执行。
    let audit = query_rows(&eng, "SELECT note FROM fxj_audit WHERE id = 1");
    assert_eq!(audit, vec![vec![s("done")]], "mode={mode:?} fxj_audit");
}

#[test]
fn should_enforce_write_buffer_immediate_policy_for_qfii_cross_table_and_fxj_close() {
    let eng = setup_engine();
    assert_buffer_policies(&eng);
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        run_qfii_and_assert(mode);
        run_fxj_and_assert(mode);
    }
}

/// `INSERT a; DELETE FROM b; INSERT b`：DELETE 执行时 b 不在缓冲里、
/// `flush_if_touches` 不会刷 a——a 的 INSERT 与后续跨表写构成耦合，
/// 必须 `Immediate`；b 的 INSERT 向前看不到该耦合，可 `Buffered`。
#[test]
fn should_keep_insert_immediate_when_other_table_dml_precedes_that_tables_insert() {
    let mut eng = setup_engine();
    eng.register(DML_ORDER_PROC).unwrap();
    let prog = &eng.catalog.lookup("prc_m1_dml_order").unwrap().prog;
    let policy_of = |prefix: &str| {
        prog.code
            .iter()
            .filter_map(|st| match st {
                IrStmt::SqlExec {
                    sql, buffer_policy, ..
                } if sql.to_ascii_lowercase().starts_with(prefix) => Some(*buffer_policy),
                _ => None,
            })
            .next()
    };
    assert_eq!(
        policy_of("insert into audit_t"),
        Some(BufferPolicy::Immediate),
        "DELETE FROM proc_log 先于 proc_log 的 INSERT，audit_t 的 INSERT 不得 Buffered"
    );
    assert_eq!(
        policy_of("insert into proc_log"),
        Some(BufferPolicy::Buffered),
        "proc_log 的 INSERT 向前扫描无耦合，应 Buffered"
    );
}
