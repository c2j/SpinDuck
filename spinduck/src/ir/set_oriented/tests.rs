//! 集合化模块级单元测试：D7 双臂独立分析/事实隔离守护（设计 v3.1 §12.3）。

use super::metadata::{ColumnMetadata, SchemaSnapshot, TableMetadata};
use super::schema::Source;
use super::{plan_from, OptimizeCtx, SetOrientedMode};
use crate::catalog::compile_sources;
use crate::rules::RuleRegistry;
use crate::types::SpinType;

/// 双臂 fixture：THEN 臂活常量 `v_yj := 'U'`，ELSE 臂 `dim_tab` 查找 +
/// CASE 投影——两臂对共享后缀变量 `v_yj` 写入事实完全不同（测试 14）。
const ROUTED: &str = "CREATE PROCEDURE copy_partial IS v_yj VARCHAR2(10); v_ct VARCHAR2(10); v_route VARCHAR2(10); BEGIN \
    FOR r IN SELECT a, k, yj_a, yj_b FROM source_rows LOOP \
    BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = 'X'; \
    EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
    BEGIN \
    IF v_route = '1' THEN v_yj := 'U'; \
    ELSE \
    SELECT ct INTO v_ct FROM dim_tab WHERE dim_tab.k = r.k; \
    SELECT CASE v_ct WHEN 'A' THEN r.yj_a WHEN 'B' THEN r.yj_b ELSE r.yj_a END INTO v_yj FROM sys_dummy; \
    END IF; \
    EXCEPTION WHEN OTHERS THEN v_yj := '0'; END; \
    INSERT INTO target_rows(val, yj) VALUES (r.a, v_yj); \
    END LOOP; END;";

fn col(name: &str, ty: SpinType) -> ColumnMetadata {
    ColumnMetadata {
        name: name.to_string(),
        source_type: String::new(),
        canonical_type: Some(ty),
        nullable: true,
        default_expr_raw: None,
    }
}

fn table(name: &str, cols: Vec<ColumnMetadata>) -> TableMetadata {
    TableMetadata {
        name: vec![name.to_string()],
        columns: cols,
        constraints: Vec::new(),
        indexes: Vec::new(),
        indexes_complete: true,
        has_opaque_ddl: false,
    }
}

fn snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![
            table(
                "source_rows",
                vec![
                    col("a", SpinType::BigInt),
                    col("k", SpinType::Varchar),
                    col("yj_a", SpinType::Varchar),
                    col("yj_b", SpinType::Varchar),
                ],
            ),
            table(
                "target_rows",
                vec![col("val", SpinType::BigInt), col("yj", SpinType::Varchar)],
            ),
            table(
                "route_tab",
                vec![col("kind", SpinType::Varchar), col("op", SpinType::Varchar)],
            ),
            table(
                "dim_tab",
                vec![col("k", SpinType::Varchar), col("ct", SpinType::Varchar)],
            ),
            table("sys_dummy", vec![col("dummy", SpinType::Varchar)]),
        ],
        diagnostics: Vec::new(),
        fingerprint: "t".into(),
    }
}

/// 测试 14（§12.6）：两臂独立走查+独立 `generate()`——臂间事实不合并。
/// THEN 臂的活常量折叠与 ELSE 臂的查找事实各自留在本臂 SQL；任一臂
/// 失败整体拒（`PlanRejection`），不存在「只证一臂」的降级计划。
#[test]
fn should_analyze_both_route_arms_with_independent_facts() {
    let rules = RuleRegistry::default();
    let compiled = compile_sources(&[ROUTED], rules.clone()).expect("例程编译");
    let routine = compiled.catalog.lookup("copy_partial").expect("例程注册");
    let snap = snapshot();
    let ctx = OptimizeCtx {
        mode: SetOrientedMode::BatchAtomic,
        batch_commit: false,
        rules: &rules,
        snapshot: Some(&snap),
        catalog: &compiled.catalog,
    };
    let plan = plan_from(routine, &Source::Static(&snap), &ctx)
        .expect("plan_from")
        .expect("双臂计划不应被拒");
    let d = plan
        .discriminator
        .as_ref()
        .expect("route 形状应产双臂判别式");
    assert_eq!(d.arms.len(), 1, "单 IF/ELSE 应恰一条非 ELSE 臂");
    let then_sql = &d.arms[0].plan.sql;
    let else_sql = &plan.sql;
    // THEN 臂独占事实：活常量 'U' 折叠进批写投影。
    assert!(then_sql.contains("'U'"), "THEN 臂缺常量折叠: {then_sql}");
    // 事实不互串：ELSE 臂的 dim_tab 查找不得出现在 THEN 臂，
    // THEN 臂的 'U' 常量不得出现在 ELSE 臂。
    assert!(
        !then_sql.contains("dim_tab"),
        "THEN 臂混入 ELSE 臂查找: {then_sql}"
    );
    assert!(else_sql.contains("dim_tab"), "ELSE 臂缺查找: {else_sql}");
    assert!(
        !else_sql.contains("'U'"),
        "ELSE 臂混入 THEN 臂常量: {else_sql}"
    );
}
