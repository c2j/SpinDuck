//! 语料 SQL 形态测试夹具（AGENTS §4.1「探针/集成」层，无容器、纯进程内）。
//!
//! 每个形态做**两个维度**的断言：
//! 1. 改写维度——与静态语句同一条管线（`ir::rewrite_sql`）的产物文本；
//! 2. 内核维度——DuckDB 1.5.5 进程内实例的实际执行结果（逐值比对）。
//!
//! PL 形态另走 `Engine::register` + `Engine::call`，即语料过程体的完整路径。

#![allow(dead_code)] // 各形态子模块只用其中一部分

use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, SqlExecutor};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::{DuckExecutor, VarScope};
use spinduck_core::value::SpinValue;
use std::path::Path;

/// 进程内内核 + 固定夹具表：每个测试新建，互不共享可变状态（§4.2）。
pub struct Kernel {
    pub ex: DuckExecutor,
    pub rules: RuleRegistry,
}

/// 夹具表：字段形态覆盖语料常用类型（VARCHAR2/NUMBER/DATE/VARCHAR 日期串）。
const FIXTURE: &str = r#"
CREATE TABLE t_form (
  id INTEGER, s VARCHAR, n DECIMAL(18,4), d DATE, ts TIMESTAMP, flag VARCHAR
);
INSERT INTO t_form VALUES
  (1, 'AbC', 12.3456, DATE '2025-07-15', TIMESTAMP '2025-07-15 10:20:30', '1'),
  (2, '  x  ', -7.5000, DATE '2025-01-31', TIMESTAMP '2025-01-31 00:00:00', '2'),
  (3, NULL, NULL, NULL, NULL, '9');
CREATE TABLE t_item (
  id INTEGER, fund VARCHAR, sec VARCHAR, kind VARCHAR, bcrq VARCHAR,
  qty DECIMAL(18,2), amt DECIMAL(18,2), fee DECIMAL(18,2)
);
INSERT INTO t_item VALUES
  (1, 'F01', 'S001', 'STOCK', '20250715', 100.00, 1200.00, 6.00),
  (2, 'F01', 'S002', 'STOCK', '20250715', 200.00, 800.00, 4.00),
  (3, 'F01', 'S001', 'BOND',  '20250716', 50.00, 5000.00, 10.00),
  (4, 'F02', 'S003', 'STOCK', '20250715', 300.00, 900.00, 4.50),
  (5, 'F02', 'S002', 'FUND',  '20250716', NULL, NULL, NULL);
CREATE TABLE t_dim (sec VARCHAR, name VARCHAR, tax DECIMAL(18,6));
INSERT INTO t_dim VALUES ('S001', '平安', 0.001000), ('S002', '招港', 0.002000);
CREATE TABLE t_valid (sec VARCHAR, kind VARCHAR, begin_c VARCHAR, end_c VARCHAR, tax DECIMAL(18,6));
INSERT INTO t_valid VALUES
  ('S001', 'STOCK', '20250701', '20250731', 0.001000),
  ('S002', 'STOCK', '20250720', '20250731', 0.002000);
CREATE TABLE t_out (id INTEGER, v VARCHAR, n DECIMAL(18,4));
CREATE TABLE sys_dummy AS SELECT 1;
"#;

impl Kernel {
    pub fn new() -> Self {
        let ex = DuckExecutor::open_in_memory().expect("open duckdb");
        let rules = RuleRegistry::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
        )
        .expect("load rules");
        for stmt in FIXTURE
            .split(";\n")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            ex.execute(stmt, &BindMap::new())
                .unwrap_or_else(|e| panic!("夹具建表失败: {e}\n{stmt}"));
        }
        Self { ex, rules }
    }

    /// 形态改写：静态语句的同一条管线（patterns → 调用点下沉 → 规则 → AST）。
    pub fn rewrite(&self, sql: &str) -> Result<String, EngineError> {
        let unit = spinduck_core::ir::rewrite_sql(
            sql,
            &VarScope::new(),
            &self.rules,
            &Default::default(),
        )?;
        Ok(unit.text)
    }

    /// 改写 + 断言产物文本（`want` 为改写后必须出现的片段）。
    pub fn rewrite_contains(&self, key: &str, sql: &str, want: &str) -> String {
        let text = self
            .rewrite(sql)
            .unwrap_or_else(|e| panic!("[{key}] 改写失败: {e}"));
        assert!(
            text.contains(want),
            "[{key}] 改写产物不含期望片段 {want:?}\n实际: {text}"
        );
        text
    }

    /// 改写 + 执行，返回归一化行集（按 SQL 自带 ORDER BY 定序）。
    pub fn rows(&self, sql: &str) -> Vec<Vec<String>> {
        let text = self.rewrite_or_panic(sql);
        let mut rs = self
            .ex
            .query(&text, &BindMap::new())
            .unwrap_or_else(|e| panic!("内核执行失败: {e}\nsql: {text}"));
        rs.fetch_many(1000)
            .iter()
            .map(|r| r.iter().map(norm_value).collect())
            .collect()
    }

    /// 改写 + 执行，取单值（行列必须唯一，否则显式 panic）。
    pub fn scalar(&self, sql: &str) -> String {
        let rows = self.rows(sql);
        assert_eq!(rows.len(), 1, "标量查询返回多行: {sql}\n{rows:?}");
        assert_eq!(rows[0].len(), 1, "标量查询返回多列: {sql}\n{rows:?}");
        rows[0][0].clone()
    }

    /// 改写 + 执行（DDL/DML），无返回值。
    pub fn exec(&self, sql: &str) {
        let text = self.rewrite_or_panic(sql);
        self.ex
            .execute(&text, &BindMap::new())
            .unwrap_or_else(|e| panic!("内核执行失败: {e}\nsql: {text}"));
    }

    /// 前置脚本：`;` 分隔逐条执行（夹具补数据用，串内不得出现分号字面量）。
    pub fn script(&self, text: &str) {
        for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            self.exec(stmt);
        }
    }

    /// 不经管线的内核原语（夹具补充：建视图前后的探查、序列等）。
    pub fn raw(&self, sql: &str) -> Vec<Vec<String>> {
        let mut rs = self
            .ex
            .query(sql, &BindMap::new())
            .unwrap_or_else(|e| panic!("内核原语失败: {e}\nsql: {sql}"));
        rs.fetch_many(1000)
            .iter()
            .map(|r| r.iter().map(norm_value).collect())
            .collect()
    }

    fn rewrite_or_panic(&self, sql: &str) -> String {
        self.rewrite(sql)
            .unwrap_or_else(|e| panic!("[形态改写失败] {e}\nsql: {sql}"))
    }
}

/// 逐值归一：与黄金 harness 同口径（忽略 scale、尾零、`+`，空串与 NULL 同值）。
///
/// 实现已收口到 `spinduck_core::norm`（A5）。这里曾是第二份逐字相同的副本，与本文件
/// 原 [`norm_value`] / [`norm_scalar`] 两个包装一起搬进 core，`compare` 才能和夹具判
/// 同一个口径；本层只引用，判定一条没改。
pub use spinduck_core::norm::{norm, norm_scalar, norm_value};

/// 进程内引擎：夹具表建好后注册例程用（语料过程体路径）。
pub fn engine() -> spinduck_core::engine::Engine {
    use spinduck_core::engine::Engine;
    let k = Kernel::new();
    let Kernel { ex, rules } = k;
    Engine::new(Box::new(ex), rules)
}

/// 注册 + 调用，返回 `CallResult`；`args` 为 SQL 字面量文本（`'x'` / `null` / 数字）。
pub fn call(
    eng: &mut spinduck_core::engine::Engine,
    routine: &str,
    args: &[&str],
) -> spinduck_core::catalog::CallResult {
    try_call(eng, routine, args).unwrap_or_else(|e| panic!("调用 {routine} 失败: {}", e.message))
}

/// 期望失败的调用：`want` 以 `ERR:` 前缀声明（断言错误消息含其后缀）。
pub fn try_call(
    eng: &mut spinduck_core::engine::Engine,
    routine: &str,
    args: &[&str],
) -> Result<spinduck_core::catalog::CallResult, spinduck_core::error::EngineError> {
    let vals: Vec<SpinValue> = args.iter().map(|a| lit_arg(a)).collect();
    eng.call(routine, &vals)
}

/// SQL 字面量形态 → SpinValue（与黄金 harness `parse_arg` 同口径）。
pub fn lit_arg(lit: &str) -> SpinValue {
    let t = lit.trim();
    if t.eq_ignore_ascii_case("null") {
        return SpinValue::Null;
    }
    if let Some(inner) = t.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return SpinValue::Str(inner.replace("''", "'"));
    }
    SpinValue::Str(t.to_string())
}

/// 被测侧读表：直接查内核（过程写入后断言输出表用）。
pub fn query(eng: &spinduck_core::engine::Engine, sql: &str) -> Vec<Vec<String>> {
    let mut rs = eng
        .ex
        .query(sql, &BindMap::new())
        .unwrap_or_else(|e| panic!("读取 {sql} 失败: {}", e.message));
    rs.fetch_many(1000)
        .iter()
        .map(|r| r.iter().map(norm_value).collect())
        .collect()
}

// ── 表驱动夹具 ──────────────────────────────────────────────────

/// 一个形态的实测文本（值/行集/改写产物统一成字符串，便于逐条报告）。
pub fn fmt_rows(rows: &[Vec<String>]) -> String {
    let cells: Vec<String> = rows.iter().map(|r| format!("[{}]", r.join(", "))).collect();
    cells.join(" ")
}

/// 期望行集 → 同一串形（避免断言两侧的表示差异）。
pub fn fmt_want(rows: &[&[&str]]) -> String {
    let cells: Vec<String> = rows.iter().map(|r| format!("[{}]", r.join(", "))).collect();
    cells.join(" ")
}

/// 探针模式：`FORMS_PROBE=1 cargo test --test corpus_sql_forms` 打印实测值而不判红，
/// 用于新增形态时取得基线；提交前必须以实际值填回 `want`（占位 `?` 会判红）。
pub fn probe_mode() -> bool {
    std::env::var("FORMS_PROBE").is_ok()
}

/// 改写维度断言：产物文本必须包含 `want` 片段（方言映射不锁定整句）。
pub fn check_contains(key: &str, want: &str, f: impl FnOnce() -> String, bad: &mut Vec<String>) {
    let actual = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if probe_mode() {
        println!("PROBE {key} = {:?}", actual.ok().as_deref());
        return;
    }
    match actual {
        Ok(a) if a.contains(want) => {}
        Ok(a) => bad.push(format!("[{key}] 产物不含 {want:?}\n      实际 {a}")),
        Err(_) => bad.push(format!("[{key}] 改写失败（见上方报错）")),
    }
}

/// 跑一批形态：`f` 返回实测文本；与 `want` 不符的收进失败清单（一次跑完全部形态）。
pub fn check(key: &str, want: &str, f: impl FnOnce() -> String, bad: &mut Vec<String>) {
    let actual = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if probe_mode() {
        println!("PROBE {key} = {:?}", actual.ok().as_deref());
        return;
    }
    if want == "?" {
        bad.push(format!("[{key}] 期望值未填写（占位 ?）"));
        return;
    }
    match actual {
        Ok(a) if a == want || want == "*" => {}
        Ok(a) => bad.push(format!("[{key}] 期望 {want}\n      实际 {a}")),
        Err(_) => bad.push(format!("[{key}] 执行/改写失败（见上方内核报错）")),
    }
}
