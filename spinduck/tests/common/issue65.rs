//! Issue #65 复现案例共享基建：BatchAtomic Skipped 回退路径的
//! lookup 成本与语义基线（mixed hit/miss + carryover 核心用例）。
//!
//! 案例形态取自 issue #65 脱敏 Case 1：游标循环内标量 `SELECT INTO`
//! + `NO_DATA_FOUND`/`TOO_MANY_ROWS` 具名处理器 + 逐行 `COMMIT`；
//!   入口守卫因 mixed miss/dup 拒绝 → 整段回退解释器逐行重跑。
//!
//! #65 复现基线分四个面（2026-11-25 实测，见各常量注释）：
//! - **A 面**（日期区间过滤）：`lookup_cache`（PR#62 遗产）已接管，
//!   内核 lookup 已是 1 次——PR 不得回退；
//! - **B 面**（数值区间过滤）：`LookupExecutor` 形态识别不接管，
//!   内核 lookup 仍是逐行 N 次——**#65 的目标面**，PR 后应为 1；
//! - **LIKE 负对照**：非 total 形态，#65 明确禁缓存——必须保持 N 次；
//! - **Case 2 Top-1 并列**：守卫拒绝整段，不加 Tie-breaker。
//!
//! 日期范围过滤用 ISO 字符串比较表达（与日期序同构，避免日期字面量
//! 方言差异；查找形态不变：等值 key + 范围过滤）。

#![cfg(feature = "native")]
#![allow(dead_code)] // 各测试 crate 只用到其中一部分（与 common/mod.rs 同惯例）

use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;
use std::cell::RefCell;
use std::rc::Rc;

pub type CallLog = Rc<RefCell<Vec<String>>>;

/// 带 SQL 文本留档的委托执行器：`query`/`execute` 文本按序记录，
/// 供内核调用次数与 INSERT/COMMIT 序列断言（§4.1 假执行器口径的
/// 真内核变体——计数 + 委托，不改变任何行为）。
pub struct CountingExecutor {
    inner: DuckExecutor,
    pub queries: CallLog,
    pub executes: CallLog,
}

impl CountingExecutor {
    pub fn open() -> Self {
        Self {
            inner: DuckExecutor::open_in_memory().expect("open in-memory duckdb"),
            queries: Rc::new(RefCell::new(Vec::new())),
            executes: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

impl SqlExecutor for CountingExecutor {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.queries.borrow_mut().push(sql.to_string());
        self.inner.query(sql, binds)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.executes.borrow_mut().push(sql.to_string());
        self.inner.execute(sql, binds)
    }
}

/// 原形态 lookup 的内核 query 计数：含 lookup 表 `asset_kind`，排除
/// 守卫聚合扫描（GROUP BY / COUNT）。
pub fn lookup_calls(queries: &CallLog) -> usize {
    queries.borrow().iter().filter(|s| is_lookup_sql(s)).count()
}

/// 原形态 lookup 判定：含 lookup 表，排除守卫聚合扫描形态。
pub fn is_lookup_sql(sql: &str) -> bool {
    let u = sql.to_ascii_uppercase();
    u.contains("ASSET_KIND") && !u.contains("GROUP BY") && !u.contains("COUNT(")
}

/// 调用前的留档水位：之后 `since(mark)` 只看本次 call 产生的文本。
pub fn executes_mark(executes: &CallLog) -> usize {
    executes.borrow().len()
}

/// mark 之后的 execute 文本序列（INSERT/COMMIT 顺序断言的原料）。
pub fn executes_since(mark: usize, executes: &CallLog) -> Vec<String> {
    executes.borrow()[mark..].to_vec()
}

/// execute 文本中 COMMIT 语句计数（逐行提交语义）。
pub fn commit_count(executes: &CallLog) -> usize {
    executes
        .borrow()
        .iter()
        .filter(|s| s.trim_start().to_ascii_uppercase().starts_with("COMMIT"))
        .count()
}

const TRADE_ROWS_DDL: &str = "CREATE TABLE trade_rows(seq BIGINT, security_id VARCHAR)";
const RESULT_ROWS_DDL: &str =
    "CREATE TABLE result_rows(seq BIGINT, security_id VARCHAR, kind VARCHAR)";
const MSG_OUT_DDL: &str = "CREATE TABLE msg_out(msg VARCHAR)";
/// A/B/C 恰一行（C 的 kind 为 NULL）；E 两行真实 duplicate；B/F 落空。
const CURSOR_SEEDS: &str =
    "INSERT INTO trade_rows VALUES (1,'A'),(2,'B'),(3,'C'),(4,'D'),(5,'E'),(6,'F')";
const LOOKUP_SEEDS: &str = "INSERT INTO asset_kind VALUES \
     ('A','K_BOND','20200101','20291231'),\
     ('C','K_FUND','20200101','20291231'),\
     ('D',NULL,'20200101','20291231'),\
     ('E','K_DUP1','20200101','20291231'),\
     ('E','K_DUP2','20200101','20291231')";

/// A 面夹具（日期区间）：issue #65 脱敏 Case 1 原形。查表命名
/// `asset_kind`（`lookup_calls` 的过滤锚点）。
pub const CASE1_SETUP: [&str; 6] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, valid_from VARCHAR, valid_to VARCHAR)",
    RESULT_ROWS_DDL,
    MSG_OUT_DDL,
    CURSOR_SEEDS,
    LOOKUP_SEEDS,
];

/// A 面例程：等值 key + 日期区间（`lookup_cache` 已接管形态）。
pub const PROC_CASE1: &str = "CREATE PROCEDURE issue65_case1 IS \
    v_kind VARCHAR2(10); p_msg VARCHAR2(100); p_biz_date VARCHAR2(8); \
    BEGIN \
    v_kind := ''; p_msg := '0'; p_biz_date := '20240601'; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id \
    AND p_biz_date BETWEEN valid_from AND valid_to; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN p_msg := 'missing:' || r.security_id; \
    WHEN TOO_MANY_ROWS THEN p_msg := 'duplicate:' || r.security_id; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    INSERT INTO msg_out VALUES (p_msg); \
    END;";

/// B 面夹具（数值区间）：同语义，过滤改为 BIGINT 列 BETWEEN——
/// `LookupExecutor` 识别器不接管的形态，#65 的目标面。
pub const CASE1_NUM_SETUP: [&str; 6] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    MSG_OUT_DDL,
    CURSOR_SEEDS,
    "INSERT INTO asset_kind VALUES \
     ('A','K_BOND',5),\
     ('C','K_FUND',5),\
     ('D',NULL,5),\
     ('E','K_DUP1',5),\
     ('E','K_DUP2',5)",
];

/// B 面例程：等值 key + 数值区间（守卫可证明 total，缓存器未接管）。
pub const PROC_CASE1_NUM: &str = "CREATE PROCEDURE issue65_case1 IS \
    v_kind VARCHAR2(10); p_msg VARCHAR2(100); \
    BEGIN \
    v_kind := ''; p_msg := '0'; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN p_msg := 'missing:' || r.security_id; \
    WHEN TOO_MANY_ROWS THEN p_msg := 'duplicate:' || r.security_id; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    INSERT INTO msg_out VALUES (p_msg); \
    END;";

/// LIKE 负对照例程：非 total 形态，#65 明确禁用缓存——内核调用必须
/// 保持逐行 N 次。输出与 A 面一致（LIKE 无通配符 = 等值）。
pub const PROC_CASE1_LIKE: &str = "CREATE PROCEDURE issue65_case1 IS \
    v_kind VARCHAR2(10); p_msg VARCHAR2(100); \
    BEGIN \
    v_kind := ''; p_msg := '0'; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id LIKE r.security_id; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN p_msg := 'missing:' || r.security_id; \
    WHEN TOO_MANY_ROWS THEN p_msg := 'duplicate:' || r.security_id; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    INSERT INTO msg_out VALUES (p_msg); \
    END;";

/// issue #65 脱敏 Case 2 例程：真实 Top-1 并列（同组两条最大
/// priority、不同 kind）。守卫必须拒绝整段，回退与 Off 一致。
pub const PROC_CASE2: &str = "CREATE PROCEDURE issue65_case2 IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    FOR r IN SELECT seq, security_id FROM tie_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM priority_kind \
    WHERE security_id = r.security_id \
    ORDER BY priority DESC; \
    EXCEPTION \
    WHEN TOO_MANY_ROWS THEN v_kind := 'TIE'; \
    END; \
    INSERT INTO tie_result VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub const CASE2_SETUP: [&str; 5] = [
    "CREATE TABLE tie_rows(seq BIGINT, security_id VARCHAR)",
    "CREATE TABLE priority_kind(security_id VARCHAR, priority BIGINT, kind VARCHAR)",
    "CREATE TABLE tie_result(seq BIGINT, security_id VARCHAR, kind VARCHAR)",
    "INSERT INTO tie_rows VALUES (1,'S1'),(2,'E'),(3,'S2')",
    // E 同组两条最大 priority、不同 kind：真实并列，首行选择有歧义。
    "INSERT INTO priority_kind VALUES \
     ('S1',5,'K_A'),('E',10,'K_X'),('E',10,'K_Y'),('S2',1,'K_C')",
];

/// 按给定建表/种子/例程组装 Case 1 族夹具。
fn setup_case1_with(setup: &[&str], proc_src: &str) -> Case1Fixture {
    let exec = CountingExecutor::open();
    let queries = exec.queries.clone();
    let executes = exec.executes.clone();
    let mut eng = Engine::new(Box::new(exec), RuleRegistry::default());
    for ddl in setup {
        eng.ex.execute(ddl, &BindMap::new()).expect("case1 DDL");
    }
    eng.register(proc_src).expect("register case1");
    Case1Fixture {
        eng,
        queries,
        executes,
    }
}

/// Case 1 族夹具：计数执行器 + 建表 + 种子 + 例程注册。
pub struct Case1Fixture {
    pub eng: Engine,
    pub queries: CallLog,
    pub executes: CallLog,
}

/// A 面（日期区间）。
pub fn setup_case1() -> Case1Fixture {
    setup_case1_with(&CASE1_SETUP, PROC_CASE1)
}

/// B 面（数值区间，#65 目标面）。
pub fn setup_case1_num() -> Case1Fixture {
    setup_case1_with(&CASE1_NUM_SETUP, PROC_CASE1_NUM)
}

/// LIKE 负对照（复用 A 面建表与种子）。
pub fn setup_case1_like() -> Case1Fixture {
    setup_case1_with(&CASE1_SETUP, PROC_CASE1_LIKE)
}

/// Case 2 夹具：同上，Top-1 并列形态。
pub struct Case2Fixture {
    pub eng: Engine,
    pub queries: CallLog,
    pub executes: CallLog,
}

pub fn setup_case2() -> Case2Fixture {
    let exec = CountingExecutor::open();
    let queries = exec.queries.clone();
    let executes = exec.executes.clone();
    let mut eng = Engine::new(Box::new(exec), RuleRegistry::default());
    for ddl in CASE2_SETUP {
        eng.ex.execute(ddl, &BindMap::new()).expect("case2 DDL");
    }
    eng.register(PROC_CASE2).expect("register case2");
    Case2Fixture {
        eng,
        queries,
        executes,
    }
}

/// Case 1 族的 Off 参考输出（issue 预期逐行表）：carryover 使 B 携带
/// 上一行命中值，恰一行 NULL 覆写后被后续 miss/dup 行保持；p_msg 尾值
/// `missing:F`。行序按 seq。三个形态变体输出一致。
pub fn expected_case1_rows() -> Vec<Vec<SpinValue>> {
    // issue 预期逐行表：carryover 使 B 携带 A 的 K_BOND；C=K_FUND；
    // D 恰一行 kind=NULL 直接写入 NULL；E/F 保持 D 的 NULL；尾 p_msg
    // = missing:F。
    const KINDS: [&str; 6] = ["K_BOND", "K_BOND", "K_FUND", "NULL", "NULL", "NULL"];
    const SECS: [&str; 6] = ["A", "B", "C", "D", "E", "F"];
    KINDS
        .iter()
        .zip(SECS)
        .enumerate()
        .map(|(i, (kind, sec))| {
            let kind = if *kind == "NULL" {
                SpinValue::Null
            } else {
                SpinValue::Str((*kind).into())
            };
            vec![
                SpinValue::Int((i + 1) as i64),
                SpinValue::Str((*sec).into()),
                kind,
            ]
        })
        .collect()
}

/// Case 1 的 Off 参考尾值：`p_msg = missing:F`。
pub fn expected_case1_msg() -> SpinValue {
    SpinValue::Str("missing:F".into())
}

/// 读取 result_rows 全量（按 seq 序；limit 供大表基准使用）。
pub fn result_rows_limit(eng: &mut Engine, limit: usize) -> Vec<Vec<SpinValue>> {
    let mut s = eng
        .ex
        .query(
            "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
            &BindMap::new(),
        )
        .expect("read result_rows");
    s.fetch_many(limit)
}

/// 读取 result_rows 全量（小夹具口径，上限 100 行）。
pub fn result_rows(eng: &mut Engine) -> Vec<Vec<SpinValue>> {
    result_rows_limit(eng, 100)
}

/// 读取 msg_out 单值（尾值 p_msg）。
pub fn msg_out(eng: &mut Engine) -> SpinValue {
    let mut s = eng
        .ex
        .query("SELECT msg FROM msg_out", &BindMap::new())
        .expect("read msg_out");
    let rows = s.fetch_many(2);
    assert_eq!(rows.len(), 1, "msg_out 恰一行");
    rows[0][0].clone()
}

/// 读取 tie_result 全量（Case 2 参考/实际比对）。
pub fn tie_rows_out(eng: &mut Engine) -> Vec<Vec<SpinValue>> {
    let mut s = eng
        .ex
        .query(
            "SELECT seq, security_id, kind FROM tie_result ORDER BY seq",
            &BindMap::new(),
        )
        .expect("read tie_result");
    s.fetch_many(100)
}

/// Case 2 的 Off 参考输出：S1 恰一行；E 真实并列 → TOO_MANY_ROWS →
/// 'TIE'；S2 恰一行。
pub fn expected_case2_rows() -> Vec<Vec<SpinValue>> {
    [(1, "S1", "K_A"), (2, "E", "TIE"), (3, "S2", "K_C")]
        .into_iter()
        .map(|(seq, sec, kind)| {
            vec![
                SpinValue::Int(seq),
                SpinValue::Str(sec.into()),
                SpinValue::Str(kind.into()),
            ]
        })
        .collect()
}

/// 基准数据播种（issue 验收口径：n 行游标，10% 恰一行命中，90% 落空；
/// duplicate 正确性由 Case 1 覆盖，不混入性能口径）。形态用 B 面
/// （数值区间）——#65 的优化目标面，A 面已被 `lookup_cache` 优化。
/// 批量 INSERT 降低播种自身成本。
pub fn setup_bench(n: usize) -> Case1Fixture {
    let exec = CountingExecutor::open();
    let queries = exec.queries.clone();
    let executes = exec.executes.clone();
    let mut eng = Engine::new(Box::new(exec), RuleRegistry::default());
    for ddl in [
        TRADE_ROWS_DDL,
        "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
        RESULT_ROWS_DDL,
    ] {
        eng.ex.execute(ddl, &BindMap::new()).expect("bench DDL");
    }
    // 游标行：i % 10 == 0 命中（S<i>），其余落空。
    for start in (0..n).step_by(1000) {
        let end = (start + 1000).min(n);
        let rows: Vec<String> = (start..end)
            .map(|i| format!("({}, '{}')", i, bench_key(i)))
            .collect();
        let sql = format!("INSERT INTO trade_rows VALUES {}", rows.join(","));
        eng.ex
            .execute(&sql, &BindMap::new())
            .expect("seed trade_rows");
    }
    let hits: Vec<String> = (0..n)
        .step_by(10)
        .map(|i| format!("('{}','K_{}',5)", bench_key(i), i))
        .collect();
    for chunk in hits.chunks(1000) {
        let sql = format!("INSERT INTO asset_kind VALUES {}", chunk.join(","));
        eng.ex
            .execute(&sql, &BindMap::new())
            .expect("seed asset_kind");
    }
    eng.register(BENCH_PROC).expect("register bench");
    Case1Fixture {
        eng,
        queries,
        executes,
    }
}

/// 基准 key：命中的 i 对应 `S<i>`，与 asset_kind 种子一致。
fn bench_key(i: usize) -> String {
    format!("S{}", i)
}

/// 基准例程：与 B 面同形态（等值 key + 数值区间 + 逐行 COMMIT），
/// 去掉消息出口，纯度量口径。
pub const BENCH_PROC: &str = "CREATE PROCEDURE issue65_bench IS \
    v_kind VARCHAR2(20); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := NULL; \
    WHEN TOO_MANY_ROWS THEN v_kind := NULL; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

// ===== issue #65 审核修复回归夹具（第二轮）=====

/// B1 回归：残余谓词含算术（`pri * pri > 0`）——不可证 total，
/// 必须整站禁用缓存；Z 行 BIGINT_MAX 使聚合预计算必然溢出。
pub const ARITH_SETUP: [&str; 5] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    "INSERT INTO trade_rows VALUES (1,'A')",
    "INSERT INTO asset_kind VALUES ('A','KA',1),('Z','KZ',9223372036854775807)",
];

pub const PROC_ARITH: &str = "CREATE PROCEDURE issue65_arith IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri * pri > 0; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_arith() -> Case1Fixture {
    setup_case1_with(&ARITH_SETUP, PROC_ARITH)
}

/// B2 回归：内外层例程同 SQL 文本、不同不变量绑定（p_biz_date）。
/// 内层 date=1 先触发预计算，外层 date=2 不得误用内层缓存。
pub const NESTED_SETUP: [&str; 6] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, valid_from VARCHAR, valid_to VARCHAR)",
    "CREATE TABLE inner_result(seq BIGINT, kind VARCHAR)",
    "CREATE TABLE outer_result(seq BIGINT, kind VARCHAR)",
    "INSERT INTO trade_rows VALUES (1,'A')",
    // 同键 'A' 两行：日期区间各自只命中一行（date=1→K2020，date=2→K2024）。
    "INSERT INTO asset_kind VALUES \
     ('A','K2020','20200101','20211231'),\
     ('A','K2024','20220101','20291231')",
];

/// 内层例程：与外层同 SQL 文本（security_id 等值键 + 日期不变量残余），
/// p_biz_date=20200101。经 `>=`/`<=` 写法避开 lookup_cache 形态识别。
pub const PROC_NESTED_INNER: &str = "CREATE PROCEDURE issue65_inner IS \
    v_kind VARCHAR2(10); p_biz_date VARCHAR2(8); \
    BEGIN \
    v_kind := ''; p_biz_date := '20200101'; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id \
    AND p_biz_date >= valid_from AND p_biz_date <= valid_to; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO inner_result VALUES (r.seq, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

/// 外层例程：先 CALL 内层（其探测先建缓存），再跑同形态循环，
/// p_biz_date=20220601。
pub const PROC_NESTED_OUTER: &str = "CREATE PROCEDURE issue65_outer IS \
    v_kind VARCHAR2(10); p_biz_date VARCHAR2(8); \
    BEGIN \
    v_kind := ''; p_biz_date := '20220601'; \
    issue65_inner(); \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id \
    AND p_biz_date >= valid_from AND p_biz_date <= valid_to; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO outer_result VALUES (r.seq, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_nested() -> Case1Fixture {
    let exec = CountingExecutor::open();
    let queries = exec.queries.clone();
    let executes = exec.executes.clone();
    let mut eng = Engine::new(Box::new(exec), RuleRegistry::default());
    for ddl in NESTED_SETUP {
        eng.ex.execute(ddl, &BindMap::new()).expect("nested DDL");
    }
    eng.register(PROC_NESTED_INNER).expect("register inner");
    eng.register(PROC_NESTED_OUTER).expect("register outer");
    Case1Fixture {
        eng,
        queries,
        executes,
    }
}

pub fn table_rows(eng: &mut Engine, sql: &str) -> Vec<Vec<SpinValue>> {
    let mut s = eng.ex.query(sql, &BindMap::new()).expect("read table");
    s.fetch_many(100)
}

/// B3 回归：首次 lookup 前 DDL 重建键列类型（VARCHAR→BIGINT）。
/// 旧 schema proof 不得复用——重建后键类型不等，站点必须重证禁用。
pub const DDL_SETUP: [&str; 5] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    "INSERT INTO trade_rows VALUES (1,'1')",
    "INSERT INTO asset_kind VALUES ('1','KA',5)",
];

pub const PROC_DDL: &str = "CREATE PROCEDURE issue65_ddl IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    CREATE OR REPLACE TABLE asset_kind AS \
    SELECT CAST(security_id AS BIGINT) AS security_id, kind, pri FROM asset_kind; \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_ddl() -> Case1Fixture {
    setup_case1_with(&DDL_SETUP, PROC_DDL)
}

/// B4 回归：DOUBLE 键经 exec_native 6 位小数有损舍入，不同键折叠。
/// 键列含浮点类型必须整站禁用缓存。
pub const DOUBLE_SETUP: [&str; 5] = [
    "CREATE TABLE trade_rows(seq BIGINT, security_id DOUBLE)",
    "CREATE TABLE asset_kind(security_id DOUBLE, kind VARCHAR, pri BIGINT)",
    "CREATE TABLE result_rows(seq BIGINT, security_id DOUBLE, kind VARCHAR)",
    "INSERT INTO trade_rows VALUES (1,1.0),(2,1.0000004)",
    "INSERT INTO asset_kind VALUES (1.0,'K0',5),(1.0000004,'K1',5)",
];

pub const PROC_DOUBLE: &str = "CREATE PROCEDURE issue65_double IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_double() -> Case1Fixture {
    setup_case1_with(&DOUBLE_SETUP, PROC_DOUBLE)
}

/// P1 回归：带别名的 B 面站点应同样吃到缓存（`FROM t AS a` + 限定列）。
pub const ALIAS_SETUP: [&str; 6] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    MSG_OUT_DDL,
    CURSOR_SEEDS,
    "INSERT INTO asset_kind VALUES \
     ('A','K_BOND',5),\
     ('C','K_FUND',5),\
     ('D',NULL,5),\
     ('E','K_DUP1',5),\
     ('E','K_DUP2',5)",
];

pub const PROC_ALIAS: &str = "CREATE PROCEDURE issue65_alias IS \
    v_kind VARCHAR2(10); p_msg VARCHAR2(100); \
    BEGIN \
    v_kind := ''; p_msg := '0'; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind AS t \
    WHERE t.security_id = r.security_id AND t.pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN p_msg := 'missing:' || r.security_id; \
    WHEN TOO_MANY_ROWS THEN p_msg := 'duplicate:' || r.security_id; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    INSERT INTO msg_out VALUES (p_msg); \
    END;";

pub fn setup_alias() -> Case1Fixture {
    setup_case1_with(&ALIAS_SETUP, PROC_ALIAS)
}
