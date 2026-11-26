//! Issue #65 审核修复回归（第二轮）：四个已复现语义问题 + 别名站点。
//!
//! - B1：残余谓词含算术 → 预计算溢出污染事务，结果必须与 Off 一致；
//! - B2：内外层例程同 SQL 文本不同不变量绑定 → 全绑定探测键隔离；
//! - B3：DDL 重建键列类型 → SiteSpec 必须重证，不得复用旧 proof；
//! - B4：DOUBLE 键有损舍入 → 键列含浮点类型整站禁用；
//! - 别名：`FROM t AS a` + 限定列的 B 面站点应同样命中缓存。

#![cfg(feature = "native")]

#[path = "common/issue65.rs"]
mod issue65;

use issue65::*;
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::value::SpinValue;

fn batch_rows(setup: fn() -> Case1Fixture, routine: &str, table_sql: &str) -> Vec<Vec<SpinValue>> {
    let mut fx = setup();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call(routine, &[]).expect("batch call");
    table_rows(&mut fx.eng, table_sql)
}

fn off_rows(setup: fn() -> Case1Fixture, routine: &str, table_sql: &str) -> Vec<Vec<SpinValue>> {
    let mut off = setup();
    off.eng.call(routine, &[]).expect("off call");
    table_rows(&mut off.eng, table_sql)
}

#[test]
fn should_issue65_b1_arith_residual_never_reaches_build() {
    // 审核复现：游标仅查 A，维度表 Z 行 pri=BIGINT_MAX，残余 `pri*pri>0`。
    // Off 与 BatchAtomic 都必须写 'KA'——算术残余必须在计划期禁用缓存，
    // 不得让预计算溢出污染事务。
    let off = off_rows(
        setup_arith,
        "issue65_arith",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(
        off,
        vec![vec![
            SpinValue::Int(1),
            SpinValue::Str("A".into()),
            SpinValue::Str("KA".into())
        ]],
        "Off 参考自检"
    );
    let batch = batch_rows(
        setup_arith,
        "issue65_arith",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(batch, off, "B1：算术残余必须禁用缓存，输出与 Off 一致");
}

#[test]
fn should_issue65_b2_nested_routine_binds_isolated() {
    // 审核复现：内层 p_biz_date=20200101 先触发预计算，外层
    // p_biz_date=20220601 不得命中内层缓存（probe key 必须含全部绑定）。
    let mut off = setup_nested();
    off.eng.call("issue65_outer", &[]).expect("off call");
    let off_inner = table_rows(
        &mut off.eng,
        "SELECT seq, kind FROM inner_result ORDER BY seq",
    );
    let off_outer = table_rows(
        &mut off.eng,
        "SELECT seq, kind FROM outer_result ORDER BY seq",
    );
    assert_eq!(
        off_inner,
        vec![vec![SpinValue::Int(1), SpinValue::Str("K2020".into())]],
        "Off 内层参考"
    );
    assert_eq!(
        off_outer,
        vec![vec![SpinValue::Int(1), SpinValue::Str("K2024".into())]],
        "Off 外层参考"
    );

    let batch = batch_rows(
        setup_nested,
        "issue65_outer",
        "SELECT seq, kind FROM outer_result ORDER BY seq",
    );
    assert_eq!(batch, off_outer, "B2：外层不得误用内层缓存");
    let mut fx = setup_nested();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_outer", &[]).expect("batch call");
    let batch_inner = table_rows(
        &mut fx.eng,
        "SELECT seq, kind FROM inner_result ORDER BY seq",
    );
    assert_eq!(batch_inner, off_inner, "B2：内层输出与 Off 一致");
}

#[test]
fn should_issue65_b3_ddl_rebuilds_site_proof() {
    // 审核复现：首次 lookup 前把 security_id 从 VARCHAR 重建为 BIGINT。
    // 站点必须丢弃旧 SiteSpec 并按新 schema 重证（类型不等 → 禁用缓存），
    // 原 SQL 隐式转换命中 'KA'，输出与 Off 一致。
    let off = off_rows(
        setup_ddl,
        "issue65_ddl",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(
        off,
        vec![vec![
            SpinValue::Int(1),
            SpinValue::Str("1".into()),
            SpinValue::Str("KA".into())
        ]],
        "Off 参考自检"
    );
    let batch = batch_rows(
        setup_ddl,
        "issue65_ddl",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(batch, off, "B3：DDL 后必须重证站点，输出与 Off 一致");
}

#[test]
fn should_issue65_b4_double_key_column_disables_cache() {
    // 审核复现：DOUBLE 键 1.0/1.0000004 经 6 位小数有损舍入折叠。
    // 键列含浮点类型必须整站禁用——两行输出与 Off 一致（均 K0）。
    let off = off_rows(
        setup_double,
        "issue65_double",
        "SELECT seq, kind FROM result_rows ORDER BY seq",
    );
    let mut fx = setup_double();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_double", &[]).expect("batch call");
    let batch = table_rows(
        &mut fx.eng,
        "SELECT seq, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(batch, off, "B4：DOUBLE 键必须禁用缓存");
    assert_eq!(
        lookup_calls(&fx.queries),
        2,
        "B4：DOUBLE 键列必须整站禁用——原形态 lookup 保持逐行"
    );
    assert_eq!(
        batch.iter().map(|r| r[1].clone()).collect::<Vec<_>>(),
        vec![SpinValue::Str("K0".into()), SpinValue::Str("K0".into())],
        "B4：两行均应命中 K0（Off 口径）"
    );
}

#[test]
fn should_issue65_alias_site_uses_cache() {
    // P1：`FROM asset_kind AS t` + `t.` 限定列的 B 面站点必须同样
    // 命中缓存（预计算恰 1 次、原形态 lookup 0 次），语义与 Off 一致。
    let mut off = setup_alias();
    off.eng.call("issue65_alias", &[]).expect("off call");
    let expected = result_rows(&mut off.eng);
    assert_eq!(expected, expected_case1_rows(), "Off 参考自检");

    let mut fx = setup_alias();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_alias", &[]).expect("batch call");
    assert_eq!(result_rows(&mut fx.eng), expected, "别名站点语义一致");
    assert_eq!(
        lookup_calls(&fx.queries),
        0,
        "别名站点：原形态内核 lookup×0（缓存必须生效）"
    );
    let agg = fx
        .queries
        .borrow()
        .iter()
        .filter(|s| {
            let u = s.to_ascii_uppercase();
            u.contains("ASSET_KIND") && (u.contains("GROUP BY") || u.contains("COUNT("))
        })
        .count();
    assert_eq!(agg, 1, "别名站点：聚合预计算恰 1 次");
}

// ===== 第三轮阻断回归 =====

#[test]
fn should_issue65_r1_mixed_type_residual_disables_and_preserves_writes() {
    // 审核 recipe：残余 `a = b`（BIGINT vs DECIMAL(38,38)）声明类型
    // 不同必须整站禁用；循环前 audit 写入不得被任何恢复动作回滚，
    // Batch execute 序列不得出现 ROLLBACK。
    let mut off = setup_mixedtype();
    off.eng.call("issue65_mixedtype", &[]).expect("off call");
    let off_rows = table_rows(
        &mut off.eng,
        "SELECT seq, kind FROM result_rows ORDER BY seq",
    );
    let off_audit = table_rows(&mut off.eng, "SELECT id FROM audit");
    assert_eq!(
        off_rows,
        vec![vec![SpinValue::Int(1), SpinValue::Str("KA".into())]],
        "Off 参考"
    );
    assert_eq!(off_audit, vec![vec![SpinValue::Int(1)]], "Off audit");

    let mut fx = setup_mixedtype();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_mixedtype", &[]).expect("batch call");
    assert_eq!(
        table_rows(
            &mut fx.eng,
            "SELECT seq, kind FROM result_rows ORDER BY seq"
        ),
        off_rows,
        "R1：跨类型残余必须禁用缓存，输出与 Off 一致"
    );
    assert_eq!(
        table_rows(&mut fx.eng, "SELECT id FROM audit"),
        off_audit,
        "R1：缓存路径不得回滚调用前未提交写入"
    );
    assert!(
        !fx.executes
            .borrow()
            .iter()
            .any(|s| s.trim_start().to_ascii_uppercase().starts_with("ROLLBACK")),
        "R1：execute 序列不得出现 ROLLBACK"
    );
    // 跨类型残余禁用 → 原形态 lookup 保持逐行。
    assert_eq!(lookup_calls(&fx.queries), 1, "R1：站点禁用走原路径");
}

#[test]
fn should_issue65_r2_expression_cursor_key_disables_cache() {
    // 审核 recipe：`security_id + 0.0 AS security_id` 表达式键——
    // verbatim 恒等映射不能证明输出类型，必须整站禁用；
    // Off/Batch 逐行一致（KA/MISS/DUP）。
    let off = off_rows(
        setup_exprkey,
        "issue65_exprkey",
        "SELECT seq, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(
        off.iter().map(|r| r[1].clone()).collect::<Vec<_>>(),
        vec![
            SpinValue::Str("KA".into()),
            SpinValue::Str("MISS".into()),
            SpinValue::Str("DUP".into())
        ],
        "Off 参考"
    );
    let mut fx = setup_exprkey();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_exprkey", &[]).expect("batch call");
    let batch = table_rows(
        &mut fx.eng,
        "SELECT seq, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(batch, off, "R2：表达式键必须禁用缓存");
    assert_eq!(
        lookup_calls(&fx.queries),
        3,
        "R2：站点禁用——原形态 lookup 保持逐行"
    );
}

#[test]
fn should_issue65_r3_scope_key_representation_bypasses_cache() {
    // 审核 recipe：外层 VARCHAR 键建成缓存后 CALL 内层同 SQL 文本
    // BIGINT 键例程——键表示不相容必须 bypass，内层 [K1,MISS,DUP]。
    let mut off = setup_scope();
    off.eng.call("issue65_scope_outer", &[]).expect("off call");
    let off_inner = table_rows(
        &mut off.eng,
        "SELECT seq, kind FROM inner_result ORDER BY seq",
    );
    assert_eq!(
        off_inner.iter().map(|r| r[1].clone()).collect::<Vec<_>>(),
        vec![
            SpinValue::Str("K1".into()),
            SpinValue::Str("MISS".into()),
            SpinValue::Str("DUP".into())
        ],
        "Off 内层参考"
    );

    let mut fx = setup_scope();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_scope_outer", &[]).expect("batch call");
    assert_eq!(
        table_rows(
            &mut fx.eng,
            "SELECT seq, kind FROM inner_result ORDER BY seq"
        ),
        off_inner,
        "R3：内层键表示不相容必须 bypass，输出与 Off 一致"
    );
    assert_eq!(
        table_rows(
            &mut fx.eng,
            "SELECT seq, kind FROM outer_result ORDER BY seq"
        ),
        table_rows(
            &mut {
                let mut o = setup_scope();
                o.eng.call("issue65_scope_outer", &[]).expect("off call");
                o.eng
            },
            "SELECT seq, kind FROM outer_result ORDER BY seq"
        ),
        "R3：外层输出与 Off 一致"
    );
}

// ===== 第三轮验收矩阵（issue #65 测试矩阵缺口） =====

fn matrix_rows(fx: &mut Case1Fixture) -> Vec<Vec<SpinValue>> {
    table_rows(
        &mut fx.eng,
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    )
}

fn s(v: &str) -> SpinValue {
    SpinValue::Str(v.into())
}

#[test]
fn should_issue65_matrix_mixed_hit_miss_dup_repeat_key() {
    // 矩阵基线：hit(KA) / miss(B) / dup(C 双行) / 重复键('A'×2)。
    let mut fx = setup_matrix();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_matrix", &[]).expect("call");
    assert_eq!(
        matrix_rows(&mut fx),
        vec![
            vec![SpinValue::Int(1), s("A"), s("KA")],
            vec![SpinValue::Int(2), s("B"), s("MISS")],
            vec![SpinValue::Int(3), s("C"), s("DUP")],
            vec![SpinValue::Int(4), s("A"), s("KA")],
        ]
    );
    assert_eq!(lookup_calls(&fx.queries), 0, "原形态 lookup 应为 0");
}

#[test]
fn should_issue65_matrix_all_miss_and_all_hit() {
    // 全 miss：维度表清空 → 合成 Missing 逐键正确。
    let mut fx = setup_matrix();
    fx.eng
        .ex
        .execute("DELETE FROM asset_kind", &Default::default())
        .expect("delete dim");
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_matrix", &[]).expect("call");
    assert!(
        matrix_rows(&mut fx).iter().all(|r| r[2] == s("MISS")),
        "全 miss：四行均应 MISS"
    );

    // 全 hit：消除 dup、补齐 B/D 键 → 每键恰一行命中。
    let mut fx = setup_matrix();
    fx.eng
        .ex
        .execute(
            "DELETE FROM asset_kind WHERE kind = 'KY'",
            &Default::default(),
        )
        .expect("dedup C");
    fx.eng
        .ex
        .execute(
            "INSERT INTO asset_kind VALUES ('B','KB',5),('D','KD',5)",
            &Default::default(),
        )
        .expect("fill keys");
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_matrix", &[]).expect("call");
    assert_eq!(
        matrix_rows(&mut fx)
            .iter()
            .map(|r| r[2].clone())
            .collect::<Vec<_>>(),
        vec![s("KA"), s("KB"), s("KX"), s("KA")],
        "全 hit：逐键恰一行命中"
    );
}

#[test]
fn should_issue65_matrix_empty_cursor_runs_no_lookup() {
    // 空游标：0 行 → 无探测、无预计算、无 lookup，输出为空。
    let mut fx = setup_matrix();
    fx.eng
        .ex
        .execute("DELETE FROM trade_rows", &Default::default())
        .expect("empty cursor");
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_matrix", &[]).expect("call");
    assert!(matrix_rows(&mut fx).is_empty());
    assert_eq!(lookup_calls(&fx.queries), 0);
    assert!(
        !fx.queries
            .borrow()
            .iter()
            .any(|q| q.to_ascii_uppercase().contains("ASSET_KIND")),
        "空游标不得发出任何 asset_kind 查询（含预计算）"
    );
}

#[test]
fn should_issue65_matrix_cross_call_invalidation() {
    // 跨调用失效：同一例程 call 两次，中间改数——每次调用区域级
    // 缓存独立重建，第二次必须反映新数据。
    let mut fx = setup_matrix();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_matrix", &[]).expect("first call");
    fx.eng
        .ex
        .execute("DELETE FROM result_rows", &Default::default())
        .expect("clear results");
    fx.eng
        .ex
        .execute(
            "UPDATE asset_kind SET kind = 'KB2' WHERE security_id = 'A' AND kind = 'KA'",
            &Default::default(),
        )
        .expect("mutate dim");
    fx.eng.call("issue65_matrix", &[]).expect("second call");
    assert_eq!(
        matrix_rows(&mut fx)
            .iter()
            .map(|r| r[2].clone())
            .collect::<Vec<_>>(),
        vec![s("KB2"), s("MISS"), s("DUP"), s("KB2")],
        "第二次调用必须按改后数据重建缓存"
    );
}

#[test]
fn should_issue65_matrix_two_lookup_sites() {
    // 多 lookup：同区域两个站点——两表各恰 1 条 GROUP BY 预计算，
    // 原形态 lookup 双双为 0，输出顺序与 Off 一致。
    let mut off = setup_multi();
    off.eng.call("issue65_multi", &[]).expect("off call");
    let off_out = table_rows(
        &mut off.eng,
        "SELECT seq, security_id, kind, rnk FROM multi_result ORDER BY seq",
    );

    let mut fx = setup_multi();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_multi", &[]).expect("batch call");
    let batch_out = table_rows(
        &mut fx.eng,
        "SELECT seq, security_id, kind, rnk FROM multi_result ORDER BY seq",
    );
    assert_eq!(batch_out, off_out, "双站点输出与 Off 一致");

    let qs = fx.queries.borrow();
    let agg = |t: &str| {
        qs.iter()
            .filter(|q| {
                let u = q.to_ascii_uppercase();
                u.contains(t) && u.contains("GROUP BY")
            })
            .count()
    };
    let orig = |t: &str| {
        qs.iter()
            .filter(|q| {
                let u = q.to_ascii_uppercase();
                u.contains(t) && !u.contains("GROUP BY") && !u.contains("COUNT(")
            })
            .count()
    };
    assert_eq!(agg("ASSET_KIND"), 1, "asset_kind 预计算恰 1 条");
    assert_eq!(agg("ASSET_RANK"), 1, "asset_rank 预计算恰 1 条");
    assert_eq!(orig("ASSET_KIND"), 0);
    assert_eq!(orig("ASSET_RANK"), 0);
}

#[test]
fn should_issue65_matrix_mutable_var_and_function_sites_disabled() {
    // 循环内被修改变量参与查找 → 禁用：原形态 lookup 逐行 N 次。
    let mut fx = setup_mutable();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_mutable", &[]).expect("call");
    assert_eq!(
        lookup_calls(&fx.queries),
        3,
        "可变依赖残余必须禁用缓存（逐行原路径）"
    );
    assert_eq!(
        matrix_rows(&mut fx)
            .iter()
            .map(|r| r[2].clone())
            .collect::<Vec<_>>(),
        vec![s("KA"), s("KB"), s("KC")]
    );

    // 函数站点（ABS(pri)）→ 禁用。
    let mut fx = setup_func();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_func", &[]).expect("call");
    assert_eq!(
        lookup_calls(&fx.queries),
        2,
        "函数残余必须禁用缓存（逐行原路径）"
    );
}

/// SqlExec 全量记录（文本 + binds）：验收要求 B 面一例逐事件比对。
type ExecEvents = std::rc::Rc<std::cell::RefCell<Vec<(String, String)>>>;

struct BindRecorder {
    events: ExecEvents,
}

impl BindRecorder {
    fn new() -> (Self, ExecEvents) {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        (
            Self {
                events: events.clone(),
            },
            events,
        )
    }
}

impl spinduck_core::exec::TraceSink for BindRecorder {
    fn on_event(&mut self, ev: spinduck_core::exec::TraceEvent) {
        if let spinduck_core::exec::TraceEvent::SqlExec { sql, binds, .. } = ev {
            self.events.borrow_mut().push((sql, format!("{binds:?}")));
        }
    }
}

#[test]
fn should_issue65_trace_lookup_events_text_and_binds_identical() {
    // B 面（issue65_case1_num）全量比对：原形态 lookup 的 SqlExec
    // 事件文本+binds 在 Off 与 BatchAtomic 下必须逐条一致。
    let mut off = setup_case1_num();
    let (mut off_rec, off_ev) = BindRecorder::new();
    off.eng
        .call_with_sink("issue65_case1", &[], &mut off_rec)
        .expect("off call");

    let mut fx = setup_case1_num();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    let (mut fx_rec, fx_ev) = BindRecorder::new();
    fx.eng
        .call_with_sink("issue65_case1", &[], &mut fx_rec)
        .expect("batch call");

    // 全量 SqlExec 序列（文本+binds）必须逐条一致——预计算经执行器
    // 内部通道下发、不产生 SqlExec 事件，故两侧事件面完全相同。
    assert_eq!(
        off_ev.borrow().as_slice(),
        fx_ev.borrow().as_slice(),
        "SqlExec 事件文本+binds+顺序必须与 Off 逐条一致"
    );
    assert!(
        fx.queries
            .borrow()
            .iter()
            .any(|q| q.to_ascii_uppercase().contains("GROUP BY")),
        "Batch 应有恰一次 GROUP BY 预计算查询"
    );
}
