//! 回归守护（AGENTS §4.3）：fn_sites 入批的语义等价性。
//!
//! 分支 3 把「非嵌套单值 fn_sites 的 INSERT VALUES」在 push 时求值例程、
//! 把缓存子查询内联为字面量后攒批。本守护锁定**批/即时两模式必须产生相同
//! 可观察行为**的语义不变量（最终行、例程异常传播 + 回滚、同表读自己写、
//! 嵌套下沉仍正确）——这些断言在重构前（即时回落）即为绿，重构后（内联
//! 攒批）必须仍绿，以此证明入批未改变语义。dispatch 机制本身的断言由
//! `runtime::interp::tests::writebuf` 的人类测试承载（已授权改写）。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;

/// 解释器/编译器递归深，测试线程给足栈（与 golden harness 同口径）。
fn big_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

fn setup(table_ddl: &str, func_ddl: &str, proc_ddl: &str) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex.execute(table_ddl, &BindMap::new()).unwrap();
    eng.register(func_ddl).unwrap();
    eng.register(proc_ddl).unwrap();
    eng
}

fn col_str(eng: &Engine, sql: &str) -> Vec<String> {
    let mut s = eng.ex.query(sql, &BindMap::new()).unwrap();
    s.fetch_many(1000)
        .iter()
        .map(|r| r[0].as_str_loose())
        .collect()
}

const FNC_DOUBLE: &str = "CREATE OR REPLACE FUNCTION fnc_double(p IN NUMBER) RETURN NUMBER IS \
     BEGIN RETURN p * 2; END;";

const PRC_FILL_DOUBLE: &str = "CREATE OR REPLACE PROCEDURE prc_fill IS \
       v_i NUMBER := 0; \
     BEGIN \
       LOOP \
         v_i := v_i + 1; \
         INSERT INTO t(n) VALUES (fnc_double(v_i)); \
         EXIT WHEN v_i >= 3; \
       END LOOP; \
     END;";

/// 数值例程：批/即时两模式最终行逐字一致（核心等价不变量）。
#[test]
fn guard_should_insert_numeric_routine_values_identically_across_batch_modes() {
    big_stack(|| {
        let mut off = setup("CREATE TABLE t(n INT)", FNC_DOUBLE, PRC_FILL_DOUBLE);
        off.batch_commit = false;
        off.call("prc_fill", &[]).unwrap();
        let mut on = setup("CREATE TABLE t(n INT)", FNC_DOUBLE, PRC_FILL_DOUBLE);
        on.batch_commit = true;
        on.call("prc_fill", &[]).unwrap();
        let expect = vec!["2".to_string(), "4".to_string(), "6".to_string()];
        assert_eq!(col_str(&off, "SELECT n FROM t ORDER BY n"), expect);
        assert_eq!(
            col_str(&on, "SELECT n FROM t ORDER BY n"),
            expect,
            "入批后最终行必须与即时模式逐字一致"
        );
    });
}

/// 字符串例程返回「形似数值的文本」（'001'）：内联必须保留字符串字面量
/// 引号——若从 VARCHAR 缓存表回读再字面量化会丢失前导零/引号信息。
#[test]
fn guard_should_preserve_string_literal_looking_like_number_across_batch_modes() {
    big_stack(|| {
        let func = "CREATE OR REPLACE FUNCTION fnc_tag(p IN NUMBER) RETURN VARCHAR IS \
             BEGIN RETURN '00' || p; END;";
        let proc = "CREATE OR REPLACE PROCEDURE prc_fill IS \
               v_i NUMBER := 0; \
             BEGIN \
               LOOP \
                 v_i := v_i + 1; \
                 INSERT INTO t(s) VALUES (fnc_tag(v_i)); \
                 EXIT WHEN v_i >= 3; \
               END LOOP; \
             END;";
        let mut off = setup("CREATE TABLE t(s VARCHAR)", func, proc);
        off.batch_commit = false;
        off.call("prc_fill", &[]).unwrap();
        let mut on = setup("CREATE TABLE t(s VARCHAR)", func, proc);
        on.batch_commit = true;
        on.call("prc_fill", &[]).unwrap();
        let expect = vec!["001".to_string(), "002".to_string(), "003".to_string()];
        assert_eq!(col_str(&off, "SELECT s FROM t ORDER BY s"), expect);
        assert_eq!(
            col_str(&on, "SELECT s FROM t ORDER BY s"),
            expect,
            "字符串字面量（含前导零）入批后必须原样保留"
        );
    });
}

/// 例程在第二个元组求值失败（除零）：批模式必须在语句处传播错误并统一
/// 回滚（半程写入不得持久化），与即时模式同口径。
#[test]
fn guard_should_propagate_routine_error_and_rollback_in_batch_mode() {
    big_stack(|| {
        let func = "CREATE OR REPLACE FUNCTION fnc_boom(p IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN 1 / (2 - p); END;";
        let proc = "CREATE OR REPLACE PROCEDURE prc_fill IS \
               v_i NUMBER := 0; \
             BEGIN \
               LOOP \
                 v_i := v_i + 1; \
                 INSERT INTO t(n) VALUES (fnc_boom(v_i)); \
                 EXIT WHEN v_i >= 3; \
               END LOOP; \
             END;";
        let mut on = setup("CREATE TABLE t(n INT)", func, proc);
        on.batch_commit = true;
        assert!(
            on.call("prc_fill", &[]).is_err(),
            "例程求值错误（p=2 除零）须外抛"
        );
        assert_eq!(
            col_str(&on, "SELECT count(*) FROM t"),
            vec!["0".to_string()],
            "未处理异常统一 ROLLBACK：半程写入不得持久化"
        );
    });
}

/// 同表先普通缓冲 INSERT、再 fn_sites INSERT：读自己写须在语句前刷盘，
/// 两模式最终行一致（缓冲行 + 内联行）。
#[test]
fn guard_should_keep_read_your_writes_for_mixed_same_table_inserts() {
    big_stack(|| {
        let proc = "CREATE OR REPLACE PROCEDURE prc_fill IS \
             BEGIN \
               INSERT INTO t(n) VALUES (1); \
               INSERT INTO t(n) VALUES (fnc_double(5)); \
             END;";
        let mut off = setup("CREATE TABLE t(n INT)", FNC_DOUBLE, proc);
        off.batch_commit = false;
        off.call("prc_fill", &[]).unwrap();
        let mut on = setup("CREATE TABLE t(n INT)", FNC_DOUBLE, proc);
        on.batch_commit = true;
        on.call("prc_fill", &[]).unwrap();
        let expect = vec!["1".to_string(), "10".to_string()];
        assert_eq!(col_str(&off, "SELECT n FROM t ORDER BY n"), expect);
        assert_eq!(
            col_str(&on, "SELECT n FROM t ORDER BY n"),
            expect,
            "同表混合插入入批后最终行必须一致"
        );
    });
}

/// 嵌套下沉 fnc_double(fnc_double(v_i))：外层元组查询引用内层缓存表，
/// 须走完整物化管线（即时回落）；批模式最终行仍与即时一致。
#[test]
fn guard_should_keep_nested_fn_sites_correct_in_batch_mode() {
    big_stack(|| {
        let proc = "CREATE OR REPLACE PROCEDURE prc_fill IS \
               v_i NUMBER := 0; \
             BEGIN \
               LOOP \
                 v_i := v_i + 1; \
                 INSERT INTO t(n) VALUES (fnc_double(fnc_double(v_i))); \
                 EXIT WHEN v_i >= 3; \
               END LOOP; \
             END;";
        let mut off = setup("CREATE TABLE t(n INT)", FNC_DOUBLE, proc);
        off.batch_commit = false;
        off.call("prc_fill", &[]).unwrap();
        let mut on = setup("CREATE TABLE t(n INT)", FNC_DOUBLE, proc);
        on.batch_commit = true;
        on.call("prc_fill", &[]).unwrap();
        let expect = vec!["4".to_string(), "8".to_string(), "12".to_string()];
        assert_eq!(col_str(&off, "SELECT n FROM t ORDER BY n"), expect);
        assert_eq!(
            col_str(&on, "SELECT n FROM t ORDER BY n"),
            expect,
            "嵌套 fn_sites 入批模式下仍须正确（即时回落）"
        );
    });
}

/// 多列 INSERT：fn_sites 列与普通绑定列共存，内联只替换缓存子查询，
/// 其余绑定列照常字面量化；两模式最终行一致。
#[test]
fn guard_should_inline_fn_site_alongside_plain_bind_columns() {
    big_stack(|| {
        let proc = "CREATE OR REPLACE PROCEDURE prc_fill IS \
               v_i NUMBER := 0; \
             BEGIN \
               LOOP \
                 v_i := v_i + 1; \
                 INSERT INTO t(n, s) VALUES (fnc_double(v_i), 'tag' || v_i); \
                 EXIT WHEN v_i >= 3; \
               END LOOP; \
             END;";
        let mut off = setup("CREATE TABLE t(n INT, s VARCHAR)", FNC_DOUBLE, proc);
        off.batch_commit = false;
        off.call("prc_fill", &[]).unwrap();
        let mut on = setup("CREATE TABLE t(n INT, s VARCHAR)", FNC_DOUBLE, proc);
        on.batch_commit = true;
        on.call("prc_fill", &[]).unwrap();
        let sql = "SELECT n || ':' || s FROM t ORDER BY n";
        let expect = vec![
            "2:tag1".to_string(),
            "4:tag2".to_string(),
            "6:tag3".to_string(),
        ];
        assert_eq!(col_str(&off, sql), expect);
        assert_eq!(
            col_str(&on, sql),
            expect,
            "fn_sites 列与普通绑定列共存时入批最终行必须一致"
        );
    });
}
