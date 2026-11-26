//! Issue #65 审核轮次回归夹具（第三/四轮）：阻断复现 + 验收矩阵。
//!
//! 与 `issue65.rs` 的 A/B/LIKE/Case2 基线夹具分离（单文件规模上限）；
//! 共享类型与构造器经 `crate::issue65` 引入。形态说明见该文件头注。

#![cfg(feature = "native")]
#![allow(dead_code)] // 各测试 crate 只用到其中一部分（与 common/mod.rs 同惯例）

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::rules::RuleRegistry;

use crate::issue65::*;

// ===== issue #65 第三轮阻断回归夹具 =====

/// 阻断1：残余 `a = b` 跨类型（BIGINT vs DECIMAL(38,38)）——同数值族
/// 但声明类型不同，必须整站禁用；循环前 audit 写入不得被任何恢复
/// 动作回滚。
pub const MIXEDTYPE_SETUP: [&str; 6] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, a BIGINT, b DECIMAL(38,38))",
    RESULT_ROWS_DDL,
    "CREATE TABLE audit(id BIGINT)",
    "INSERT INTO trade_rows VALUES (1,'A')",
    // Z 行 a=BIGINT_MAX：DECIMAL(38,38) 无整数位，聚合预计算 cast 必炸。
    "INSERT INTO asset_kind VALUES ('A','KA',0,0),('Z','KZ',9223372036854775807,0)",
];

pub const PROC_MIXEDTYPE: &str = "CREATE PROCEDURE issue65_mixedtype IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    INSERT INTO audit VALUES (1); \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND a = b; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_mixedtype() -> Case1Fixture {
    setup_case1_with(&MIXEDTYPE_SETUP, PROC_MIXEDTYPE)
}

/// 阻断2：表达式游标键 `security_id + 0.0`——verbatim 恒等映射
/// 不能证明输出类型，必须整站禁用（运行期键为 Decimal）。
pub const EXPRKEY_SETUP: [&str; 5] = [
    "CREATE TABLE trade_rows(seq BIGINT, security_id BIGINT)",
    "CREATE TABLE asset_kind(security_id BIGINT, kind VARCHAR, pri BIGINT)",
    "CREATE TABLE result_rows(seq BIGINT, security_id BIGINT, kind VARCHAR)",
    "INSERT INTO trade_rows VALUES (1,1),(2,2),(3,3)",
    "INSERT INTO asset_kind VALUES (1,'KA',5),(3,'KX',5),(3,'KY',5)",
];

pub const PROC_EXPRKEY: &str = "CREATE PROCEDURE issue65_exprkey IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id + 0.0 AS security_id \
    FROM trade_rows ORDER BY seq LOOP \
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

pub fn setup_exprkey() -> Case1Fixture {
    setup_case1_with(&EXPRKEY_SETUP, PROC_EXPRKEY)
}

/// 阻断3：外层 VARCHAR 键建成缓存后 CALL 内层同 SQL 文本 BIGINT 键
/// 例程——探测键表示不相容必须 bypass，内层输出与 Off 一致。
pub const SCOPE_SETUP: [&str; 6] = [
    "CREATE TABLE outer_rows(seq BIGINT, security_id VARCHAR)",
    "CREATE TABLE inner_rows(seq BIGINT, security_id BIGINT)",
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    "CREATE TABLE outer_result(seq BIGINT, kind VARCHAR)",
    "CREATE TABLE inner_result(seq BIGINT, kind VARCHAR)",
    // 外层键全 miss（建成全 Missing 缓存）；内层 BIGINT 1/2/3 → K1/MISS/DUP。
    "INSERT INTO outer_rows VALUES (1,'x'),(2,'y'),(3,'z');\
     INSERT INTO inner_rows VALUES (1,1),(2,2),(3,3);\
     INSERT INTO asset_kind VALUES ('1','K1',5),('3','KX',5),('3','KY',5)",
];

pub const PROC_SCOPE_INNER: &str = "CREATE PROCEDURE issue65_scope_inner IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM inner_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO inner_result VALUES (r.seq, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub const PROC_SCOPE_OUTER: &str = "CREATE PROCEDURE issue65_scope_outer IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM outer_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO outer_result VALUES (r.seq, v_kind); \
    COMMIT; \
    END LOOP; \
    issue65_scope_inner(); \
    END;";

pub fn setup_scope() -> Case1Fixture {
    let exec = CountingExecutor::open();
    let queries = exec.queries.clone();
    let executes = exec.executes.clone();
    let mut eng = Engine::new(Box::new(exec), RuleRegistry::default());
    for ddl in SCOPE_SETUP {
        eng.ex.execute(ddl, &BindMap::new()).expect("scope DDL");
    }
    eng.register(PROC_SCOPE_INNER).expect("register inner");
    eng.register(PROC_SCOPE_OUTER).expect("register outer");
    Case1Fixture {
        eng,
        queries,
        executes,
    }
}

// ===== issue #65 第三轮验收矩阵夹具 =====

/// 矩阵：混合 hit/miss/dup + 重复键（'A' 出现两次）——全 miss/全 hit/
/// 空游标由测试在新鲜 fixture 上改数据构造。
pub const MATRIX_SETUP: [&str; 5] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    "INSERT INTO trade_rows VALUES (1,'A'),(2,'B'),(3,'C'),(4,'A')",
    "INSERT INTO asset_kind VALUES ('A','KA',5),('C','KX',5),('C','KY',5)",
];

pub const PROC_MATRIX: &str = "CREATE PROCEDURE issue65_matrix IS \
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

pub fn setup_matrix() -> Case1Fixture {
    setup_case1_with(&MATRIX_SETUP, PROC_MATRIX)
}

/// 多 lookup：同一区域两个可证站点（asset_kind + asset_rank），
/// 输出顺序与两次预计算各自恰 1 条。
pub const MULTI_SETUP: [&str; 6] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    "CREATE TABLE asset_rank(security_id VARCHAR, rnk VARCHAR, pri BIGINT)",
    "CREATE TABLE multi_result(seq BIGINT, security_id VARCHAR, kind VARCHAR, rnk VARCHAR)",
    "INSERT INTO trade_rows VALUES (1,'A'),(2,'B')",
    "INSERT INTO asset_kind VALUES ('A','KA',5);\
     INSERT INTO asset_rank VALUES ('A','R1',5),('B','R2',5)",
];

pub const PROC_MULTI: &str = "CREATE PROCEDURE issue65_multi IS \
    v_kind VARCHAR2(10); v_rnk VARCHAR2(10); \
    BEGIN \
    v_kind := ''; v_rnk := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    BEGIN \
    SELECT rnk INTO v_rnk FROM asset_rank \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_rnk := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_rnk := 'DUP'; \
    END; \
    INSERT INTO multi_result VALUES (r.seq, r.security_id, v_kind, v_rnk); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_multi() -> Case1Fixture {
    setup_case1_with(&MULTI_SETUP, PROC_MULTI)
}

/// 可变依赖禁用：残余 `pri >= v_lo` 的 v_lo 在循环内被修改——
/// 参与查找的变量非常量，必须禁用缓存走原路径。
pub const MUTABLE_SETUP: [&str; 5] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    "INSERT INTO trade_rows VALUES (1,'A'),(2,'B'),(3,'C')",
    "INSERT INTO asset_kind VALUES ('A','KA',1),('B','KB',2),('C','KC',3)",
];

pub const PROC_MUTABLE: &str = "CREATE PROCEDURE issue65_mutable IS \
    v_kind VARCHAR2(10); v_lo BIGINT; \
    BEGIN \
    v_kind := ''; v_lo := 1; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri >= v_lo; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    v_lo := v_lo + 1; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_mutable() -> Case1Fixture {
    setup_case1_with(&MUTABLE_SETUP, PROC_MUTABLE)
}

/// 函数站点禁用：残余含 `ABS(pri)`——非全定义可证形态，整站禁用。
pub const FUNC_SETUP: [&str; 5] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    "INSERT INTO trade_rows VALUES (1,'A'),(2,'B')",
    "INSERT INTO asset_kind VALUES ('A','KA',5),('B','KB',5)",
];

pub const PROC_FUNC: &str = "CREATE PROCEDURE issue65_func IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND ABS(pri) > 0; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_func() -> Case1Fixture {
    setup_case1_with(&FUNC_SETUP, PROC_FUNC)
}

// ===== issue #65 第四轮阻断回归夹具 =====

/// R4-1：会话 `default_collation='nocase'`——GROUP BY 键折叠
/// （'A'/'a' 同组）与探测 map 精确匹配口径不一致，必须整站禁用。
/// 游标键小写、维度键大写：Off 下 nocase 比较仍命中。
pub const COLLATE_SETUP: [&str; 5] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT)",
    RESULT_ROWS_DDL,
    "INSERT INTO trade_rows VALUES (1,'a'),(2,'b')",
    "INSERT INTO asset_kind VALUES ('A','KA',5)",
];

pub const PROC_COLLATE: &str = "CREATE PROCEDURE issue65_collate IS \
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

pub fn setup_collate() -> Case1Fixture {
    setup_case1_with(&COLLATE_SETUP, PROC_COLLATE)
}

/// R4-1b：例程内 EXECUTE IMMEDIATE 改会话排序规则——写失效之外
/// 的会话态变化必须使站点规格重证（重证时 collation 复核失败禁用）。
pub const PROC_COLLATE_MID: &str = "CREATE PROCEDURE issue65_collate_mid IS \
    v_kind VARCHAR2(10); v_i INT; \
    BEGIN \
    v_kind := ''; v_i := 0; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    v_i := v_i + 1; \
    IF v_i = 2 THEN \
    EXECUTE IMMEDIATE 'SET default_collation = ''nocase'''; \
    END IF; \
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

pub fn setup_collate_mid() -> Case1Fixture {
    setup_case1_with(&COLLATE_SETUP, PROC_COLLATE_MID)
}

/// R4-2：`SELECT DISTINCT kind` 被 shape::select 误解析为列
/// `distinct`+别名 `kind`；存在同名引号列 `"distinct"` 时投影证明
/// 通过、聚合投影错误列。必须显式拒绝 DISTINCT/ALL。
pub const DISTINCT_SETUP: [&str; 5] = [
    TRADE_ROWS_DDL,
    "CREATE TABLE asset_kind(security_id VARCHAR, kind VARCHAR, pri BIGINT, \"distinct\" VARCHAR)",
    RESULT_ROWS_DDL,
    "INSERT INTO trade_rows VALUES (1,'A'),(2,'A')",
    "INSERT INTO asset_kind VALUES ('A','KA',5,'WRONG')",
];

pub const PROC_DISTINCT: &str = "CREATE PROCEDURE issue65_distinct IS \
    v_kind VARCHAR2(10); \
    BEGIN \
    v_kind := ''; \
    FOR r IN SELECT seq, security_id FROM trade_rows ORDER BY seq LOOP \
    BEGIN \
    SELECT DISTINCT kind INTO v_kind FROM asset_kind \
    WHERE security_id = r.security_id AND pri BETWEEN 1 AND 9; \
    EXCEPTION \
    WHEN NO_DATA_FOUND THEN v_kind := 'MISS'; \
    WHEN TOO_MANY_ROWS THEN v_kind := 'DUP'; \
    END; \
    INSERT INTO result_rows VALUES (r.seq, r.security_id, v_kind); \
    COMMIT; \
    END LOOP; \
    END;";

pub fn setup_distinct() -> Case1Fixture {
    setup_case1_with(&DISTINCT_SETUP, PROC_DISTINCT)
}
