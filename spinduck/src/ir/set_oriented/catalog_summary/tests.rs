//! 跨例程摘要、纯调用与空桩边界守护。

use super::*;
use crate::catalog::{compile_statement_routine, CompiledRoutine};
use crate::rules::RuleRegistry;

fn compile(sql: &str) -> CompiledRoutine {
    let tokens = ogsql_parser::token::tokenizer::Tokenizer::new(sql)
        .tokenize()
        .unwrap();
    let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
    compile_statement_routine(&stmts[0], &RuleRegistry::default())
        .unwrap()
        .remove(0)
}

fn catalog_with(routines: &[CompiledRoutine]) -> RoutineCatalog {
    let mut cat = RoutineCatalog::default();
    for r in routines {
        cat.register(r.clone());
    }
    cat
}

fn noop_inout() -> CompiledRoutine {
    compile("CREATE PROCEDURE p_noop(p_x IN OUT VARCHAR2) AS BEGIN NULL; END;")
}

fn caller(var_ty: SpinType) -> IrProg {
    let mut prog = IrProg::default();
    prog.var_types.insert("v".to_string(), var_ty);
    prog
}

#[test]
fn should_treat_builtin_function_call_as_pure_in_summary() {
    // 内建函数（SUBSTRING 等未注册名）解释器下沉 SELECT f(...) 求值，无 PL
    // 副作用，须判纯（与 runtime::eval::eval_funccall 单次精确查找同口径）；
    // 可渲染性由 inline_safe 白名单另行把关，不在摘要层误拒。
    let f = compile(
        "CREATE FUNCTION f_sub(p_x VARCHAR2) RETURN VARCHAR2 IS BEGIN \
         RETURN substr(p_x, 1, 3); END;",
    );
    let cat = catalog_with(&[f]);
    assert!(
        matches!(summarize(&cat, "f_sub"), Summary::PureExpr(_)),
        "{:?}",
        summarize(&cat, "f_sub")
    );
    // 内联展开须保留内建 FuncCall（仅展开实参），并可渲染为标量 SQL。
    let args = [IrExpr::Field {
        rec: super::super::VarName("r".into()),
        field: "security_id".into(),
    }];
    let inlined = inline_pure_call(&cat, "f_sub", &args)
        .map(|call| call.expr)
        .expect("inline builtin");
    let (sql, _) = crate::sql::render::expr_to_scalar_sql_binds(&inlined).expect("render builtin");
    assert!(sql.contains("SUBSTRING"), "须保留内建调用: {sql}");
    assert!(!sql.contains("$p0"), "形参占位符须被实参替换: {sql}");
}

#[test]
fn should_not_treat_noop_body_with_inout_coercion_as_noop() {
    // 设计 §6.2：v NUMBER := 21 传入 IN OUT VARCHAR2 空过程后
    // 写回 '21'，v < '3' 由假变真——值可观察，NoOp 必须拒绝消除。
    let cat = catalog_with(&[noop_inout()]);
    let prog = caller(SpinType::Decimal {
        precision: 3,
        scale: 0,
    });
    let args = [IrExpr::Var(super::super::VarName("v".into()))];
    let outs = [Some(AssignTarget::Var(super::super::VarName("v".into())))];
    assert!(!call_is_noop(&cat, &prog, "p_noop", &args, &outs));
}

#[test]
fn should_accept_noop_call_with_varchar_args_and_lossless_writeback() {
    let cat = catalog_with(&[noop_inout()]);
    let prog = caller(SpinType::Varchar);
    let v = super::super::VarName("v".into());
    // 字面量 / VARCHAR 变量 / 含非空字面量的拼接均可证 Str/Null。
    for arg in [
        IrExpr::Const(SpinValue::Str("x".into())),
        IrExpr::Var(v.clone()),
        IrExpr::Bin {
            op: BinOp::Concat,
            l: Box::new(IrExpr::Const(SpinValue::Str("code=".into()))),
            r: Box::new(IrExpr::Field {
                rec: super::super::VarName("r".into()),
                field: "gddm".into(),
            }),
        },
    ] {
        let outs = [Some(AssignTarget::Var(v.clone()))];
        assert!(call_is_noop(&cat, &prog, "p_noop", &[arg], &outs));
    }
}

#[test]
fn should_accept_noop_call_with_sqlerrm_terminated_concat_arg() {
    // 跳行 trap 处理器里的日志桩调用：p_msg 实参以 SQLERRM 收尾
    // （'取' || r.payload || SQLERRM）。SQLERRM 编译为隐式错误帧变量
    // __sqlerrm（VARCHAR，处理器内恒 Str），未声明于 var_types；NoOp
    // 证明须承认其为 Str/Null，否则日志桩无法消除 → UnsupportedCall。
    let cat = catalog_with(&[noop_inout()]);
    let prog = caller(SpinType::Varchar);
    let v = super::super::VarName("v".into());
    let arg = IrExpr::Bin {
        op: BinOp::Concat,
        l: Box::new(IrExpr::Bin {
            op: BinOp::Concat,
            l: Box::new(IrExpr::Const(SpinValue::Str("取".into()))),
            r: Box::new(IrExpr::Field {
                rec: super::super::VarName("r".into()),
                field: "payload".into(),
            }),
        }),
        r: Box::new(IrExpr::Var(super::super::VarName("__sqlerrm".into()))),
    };
    let outs = [Some(AssignTarget::Var(v.clone()))];
    assert!(call_is_noop(&cat, &prog, "p_noop", &[arg], &outs));
}

#[test]
fn should_reject_noop_call_with_non_varchar_writeback_slot_or_arity_mismatch() {
    let cat = catalog_with(&[noop_inout()]);
    let prog = caller(SpinType::Varchar);
    let v = super::super::VarName("v".into());
    // 写回目标是记录字段：不可证恒等。
    let outs = [Some(AssignTarget::Field {
        rec: v.clone(),
        field: "f".into(),
    })];
    assert!(!call_is_noop(
        &cat,
        &prog,
        "p_noop",
        &[IrExpr::Const(SpinValue::Str("x".into()))],
        &outs
    ));
    // 实参个数与形参不符（默认值覆盖路径）：拒绝。
    assert!(!call_is_noop(&cat, &prog, "p_noop", &[], &[]));
}

#[test]
fn should_summarize_noop_procedure_const_function_and_unregistered() {
    let noop = compile("CREATE PROCEDURE p_null AS BEGIN NULL; END;");
    let f = compile("CREATE FUNCTION f_zero(p_a VARCHAR2) RETURN NUMBER IS BEGIN RETURN 0; END;");
    let impure = compile("CREATE PROCEDURE p_write AS BEGIN INSERT INTO t VALUES (1); END;");
    let cat = catalog_with(&[noop, f, impure]);
    assert!(matches!(summarize(&cat, "p_null"), Summary::NoOp));
    match summarize(&cat, "f_zero") {
        Summary::PureExpr(InlinedCall {
            expr: IrExpr::Const(SpinValue::Int(0)),
            ..
        }) => {}
        other => panic!("expected PureExpr(Const 0), got {other:?}"),
    }
    assert!(matches!(summarize(&cat, "p_write"), Summary::Impure));
    assert!(matches!(summarize(&cat, "absent"), Summary::Unregistered));
}

#[test]
fn should_summarize_recursive_rings_as_impure_and_expand_acyclic_pure_calls() {
    // 防递归环（设计 §4.3）：自递归/互递归体必含 Nop/Return 之外指令或
    // 被 visiting 环检测拒绝 → Impure，绝不无限展开；无环纯嵌套调用则
    // 递归判纯（f_call 体调用纯函数 f_getday）。
    let self_rec = compile(
        "CREATE FUNCTION f_rec(p_a NUMBER) RETURN NUMBER IS BEGIN \
        IF p_a > 0 THEN RETURN f_rec(p_a - 1); END IF; RETURN 0; END;",
    );
    let mut_a = compile("CREATE PROCEDURE p_a AS BEGIN p_b(); END;");
    let mut_b = compile("CREATE PROCEDURE p_b AS BEGIN p_a(); END;");
    let getday = compile(
        "CREATE FUNCTION f_getday(p_d VARCHAR2, p_n NUMBER) RETURN VARCHAR2 IS BEGIN \
        RETURN to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); END;",
    );
    let f_call = compile(
        "CREATE FUNCTION f_call(p_a NUMBER) RETURN VARCHAR2 IS BEGIN \
        RETURN f_getday('20250715', p_a); END;",
    );
    let cat = catalog_with(&[self_rec, mut_a, mut_b, getday, f_call]);
    assert!(matches!(summarize(&cat, "f_rec"), Summary::Impure));
    assert!(matches!(summarize(&cat, "p_a"), Summary::Impure));
    assert!(matches!(summarize(&cat, "p_b"), Summary::Impure));
    assert!(matches!(summarize(&cat, "f_call"), Summary::PureExpr(_)));
    assert!(!call_is_noop(&cat, &IrProg::default(), "p_a", &[], &[]));
}

#[test]
fn should_inline_pure_expression_call_substituting_arguments() {
    // 纯计算函数（SqlFn 日期算术 + Bin）摘要为 PureExpr(非常量)，
    // inline_pure_call 将形参替换为实参；无环嵌套纯调用递归判纯。
    let getday = compile(
        "CREATE FUNCTION f_getday(p_d VARCHAR2, p_n NUMBER) RETURN VARCHAR2 IS BEGIN \
        RETURN to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); END;",
    );
    let nested = compile(
        "CREATE FUNCTION f_call(p_a NUMBER) RETURN NUMBER IS BEGIN \
        RETURN f_getday('20250715', p_a); END;",
    );
    let cat = catalog_with(&[getday, nested]);
    assert!(matches!(summarize(&cat, "f_getday"), Summary::PureExpr(_)));
    assert!(matches!(summarize(&cat, "f_call"), Summary::PureExpr(_)));
    let args = [
        IrExpr::Field {
            rec: super::super::VarName("r".into()),
            field: "bcrq".into(),
        },
        IrExpr::Const(SpinValue::Int(1)),
    ];
    let inlined = inline_pure_call(&cat, "f_getday", &args)
        .map(|call| call.expr)
        .expect("inline");
    // 形参 p_d 已被记录字段实参替换：体内不再残留 p_d 变量，
    // 且出现实参记录字段 r.bcrq。
    fn contains(e: &IrExpr, pred: &dyn Fn(&IrExpr) -> bool) -> bool {
        if pred(e) {
            return true;
        }
        match e {
            IrExpr::Bin { l, r, .. } => contains(l, pred) || contains(r, pred),
            IrExpr::SqlFn { args, .. } => args.iter().any(|a| contains(a, pred)),
            _ => false,
        }
    }
    let is_p_d = |e: &IrExpr| matches!(e, IrExpr::Var(v) if v.0.eq_ignore_ascii_case("p_d"));
    let is_r_bcrq =
        |e: &IrExpr| matches!(e, IrExpr::Field { rec, field } if rec.0 == "r" && field == "bcrq");
    assert!(!contains(&inlined, &is_p_d));
    assert!(contains(&inlined, &is_r_bcrq));
    // 元数不符 → 拒绝。
    assert!(inline_pure_call(&cat, "f_getday", &args[..1]).is_none());
}

#[test]
fn should_inline_nested_pure_calls_recursively() {
    // 嵌套纯调用（§4.3 A7）：f_outer 体含 FuncCall f_getday，须递归展开
    // 为无 FuncCall 的纯表达式，形参 p_d 替换为记录字段实参。
    let getday = compile(
        "CREATE FUNCTION f_getday(p_d VARCHAR2, p_n NUMBER) RETURN VARCHAR2 IS BEGIN \
        RETURN to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); END;",
    );
    let outer = compile(
        "CREATE FUNCTION f_outer(p_d VARCHAR2) RETURN VARCHAR2 IS BEGIN \
        RETURN f_getday(p_d, 1); END;",
    );
    let cat = catalog_with(&[getday, outer]);
    assert!(matches!(summarize(&cat, "f_outer"), Summary::PureExpr(_)));
    let args = [IrExpr::Field {
        rec: super::super::VarName("r".into()),
        field: "bcrq".into(),
    }];
    let inlined = inline_pure_call(&cat, "f_outer", &args)
        .map(|call| call.expr)
        .expect("inline nested");
    fn contains(e: &IrExpr, pred: &dyn Fn(&IrExpr) -> bool) -> bool {
        if pred(e) {
            return true;
        }
        match e {
            IrExpr::Bin { l, r, .. } => contains(l, pred) || contains(r, pred),
            IrExpr::SqlFn { args, .. } | IrExpr::FuncCall { args, .. } => {
                args.iter().any(|a| contains(a, pred))
            }
            IrExpr::Not(x) | IrExpr::IsNull(x) => contains(x, pred),
            _ => false,
        }
    }
    let is_funccall = |e: &IrExpr| matches!(e, IrExpr::FuncCall { .. });
    let is_p_d = |e: &IrExpr| matches!(e, IrExpr::Var(v) if v.0.eq_ignore_ascii_case("p_d"));
    let is_r_bcrq =
        |e: &IrExpr| matches!(e, IrExpr::Field { rec, field } if rec.0 == "r" && field == "bcrq");
    assert!(
        !contains(&inlined, &is_funccall),
        "FuncCall 须被展开: {inlined:?}"
    );
    assert!(!contains(&inlined, &is_p_d));
    assert!(contains(&inlined, &is_r_bcrq));
}

#[test]
fn should_inline_function_internal_select_into_as_count_gated_lookup() {
    // 函数内 SELECT INTO + WHEN OTHERS 兜底（§4.3 A7 LookupExpr）：摘要为
    // 含 Lookup 的 PureExpr；内联后形参替换为记录字段实参；标量 SQL 渲染为
    // 计数门控相关子查询（0/>1 行→'0'，恰 1 行→值）。
    let f = compile(
        "CREATE FUNCTION f_lk(p_id VARCHAR2, p_d VARCHAR2) RETURN VARCHAR2 IS \
         v_days VARCHAR2(8); \
         BEGIN \
         BEGIN \
         SELECT t.purchase_days INTO v_days FROM par_sys_purchase t \
         WHERE t.security_id = p_id AND p_d BETWEEN t.inure_begin_date AND t.inure_end_date; \
         EXCEPTION WHEN OTHERS THEN v_days := '0'; \
         END; \
         RETURN v_days; END;",
    );
    let cat = catalog_with(&[f]);
    assert!(matches!(summarize(&cat, "f_lk"), Summary::PureExpr(_)));
    let args = [
        IrExpr::Field {
            rec: super::super::VarName("r".into()),
            field: "security_id".into(),
        },
        IrExpr::Field {
            rec: super::super::VarName("r".into()),
            field: "purchase_date".into(),
        },
    ];
    let inlined = inline_pure_call(&cat, "f_lk", &args)
        .map(|call| call.expr)
        .expect("inline lookup");
    assert!(
        matches!(inlined, IrExpr::Lookup { .. }),
        "须折叠为 Lookup: {inlined:?}"
    );
    let (sql, binds) =
        crate::sql::render::expr_to_scalar_sql_binds(&inlined).expect("render lookup scalar");
    assert!(sql.contains("COUNT(*) = 1"), "须计数门控: {sql}");
    assert!(sql.contains("MIN(sd_lk_val)"), "须单聚合取值: {sql}");
    assert!(sql.contains("AS sd_lk(sd_lk_val)"), "须派生表列别名: {sql}");
    assert!(sql.contains("'0'"), "须兜底默认: {sql}");
    assert!(sql.contains("par_sys_purchase"), "须查找表: {sql}");
    // 形参占位符已被记录字段实参替换，绑定源指向记录字段。
    assert!(!sql.contains("$v_p_id"), "形参占位符须被替换: {sql}");
    assert!(
        binds.iter().any(|(_, s)| matches!(s,
            super::super::BindSrc::Field { rec, field } if rec.0 == "r" && field == "security_id")),
        "须收集记录字段绑定: {binds:?}"
    );
}

#[test]
fn should_fold_if_then_conditional_assignment_into_case() {
    // IF-THEN 无 ELSE 条件重赋值（§4.3 A7）：折叠为
    // CASE WHEN cond THEN then_val ELSE prior END，局部变量代入消去。
    let f = compile(
        "CREATE FUNCTION f_if(p_d VARCHAR2, p_n NUMBER) RETURN VARCHAR2 IS \
         v_x VARCHAR2(8); v_flag NUMBER; \
         BEGIN \
         v_x := to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); \
         v_flag := 1; \
         IF v_flag = 1 THEN v_x := to_char(to_date(p_d, 'YYYYMMDD') + 2, 'YYYYMMDD'); END IF; \
         RETURN v_x; END;",
    );
    let cat = catalog_with(&[f]);
    assert!(matches!(summarize(&cat, "f_if"), Summary::PureExpr(_)));
    let args = [
        IrExpr::Field {
            rec: super::super::VarName("r".into()),
            field: "bcrq".into(),
        },
        IrExpr::Const(SpinValue::Int(1)),
    ];
    let inlined = inline_pure_call(&cat, "f_if", &args)
        .map(|call| call.expr)
        .expect("inline if-fold");
    assert!(
        matches!(inlined, IrExpr::Case { .. }),
        "条件重赋值须折叠为 CASE: {inlined:?}"
    );
    fn contains(e: &IrExpr, pred: &dyn Fn(&IrExpr) -> bool) -> bool {
        if pred(e) {
            return true;
        }
        match e {
            IrExpr::Bin { l, r, .. } => contains(l, pred) || contains(r, pred),
            IrExpr::SqlFn { args, .. } | IrExpr::FuncCall { args, .. } => {
                args.iter().any(|a| contains(a, pred))
            }
            IrExpr::Case {
                operand,
                whens,
                else_expr,
            } => {
                operand.as_deref().is_some_and(|x| contains(x, pred))
                    || whens
                        .iter()
                        .any(|(c, v)| contains(c, pred) || contains(v, pred))
                    || else_expr.as_deref().is_some_and(|x| contains(x, pred))
            }
            IrExpr::Not(x) | IrExpr::IsNull(x) => contains(x, pred),
            _ => false,
        }
    }
    let is_local = |e: &IrExpr| matches!(e, IrExpr::Var(v) if v.0.eq_ignore_ascii_case("v_x") || v.0.eq_ignore_ascii_case("v_flag"));
    let is_r_bcrq =
        |e: &IrExpr| matches!(e, IrExpr::Field { rec, field } if rec.0 == "r" && field == "bcrq");
    assert!(!contains(&inlined, &is_local), "局部须被消去: {inlined:?}");
    assert!(contains(&inlined, &is_r_bcrq));
}

#[test]
fn should_inline_straight_line_assignment_function_body() {
    // 多语句函数体（§4.3 A7）：顺序纯赋值 + 末 RETURN 折叠为单表达式，
    // 局部变量经代入消去，形参替换为记录字段实参后无残留局部/FuncCall。
    let f = compile(
        "CREATE FUNCTION f_calc(p_d VARCHAR2) RETURN VARCHAR2 IS \
         v_x VARCHAR2(8); \
         BEGIN \
         v_x := to_char(to_date(p_d, 'YYYYMMDD') + 1, 'YYYYMMDD'); \
         RETURN v_x; END;",
    );
    let cat = catalog_with(&[f]);
    assert!(matches!(summarize(&cat, "f_calc"), Summary::PureExpr(_)));
    let args = [IrExpr::Field {
        rec: super::super::VarName("r".into()),
        field: "bcrq".into(),
    }];
    let inlined = inline_pure_call(&cat, "f_calc", &args)
        .map(|call| call.expr)
        .expect("inline straight-line");
    fn contains(e: &IrExpr, pred: &dyn Fn(&IrExpr) -> bool) -> bool {
        if pred(e) {
            return true;
        }
        match e {
            IrExpr::Bin { l, r, .. } => contains(l, pred) || contains(r, pred),
            IrExpr::SqlFn { args, .. } | IrExpr::FuncCall { args, .. } => {
                args.iter().any(|a| contains(a, pred))
            }
            IrExpr::Not(x) | IrExpr::IsNull(x) => contains(x, pred),
            _ => false,
        }
    }
    let is_v_x = |e: &IrExpr| matches!(e, IrExpr::Var(v) if v.0.eq_ignore_ascii_case("v_x"));
    let is_p_d = |e: &IrExpr| matches!(e, IrExpr::Var(v) if v.0.eq_ignore_ascii_case("p_d"));
    let is_r_bcrq =
        |e: &IrExpr| matches!(e, IrExpr::Field { rec, field } if rec.0 == "r" && field == "bcrq");
    assert!(
        !contains(&inlined, &is_v_x),
        "局部 v_x 须被代入消去: {inlined:?}"
    );
    assert!(!contains(&inlined, &is_p_d));
    assert!(contains(&inlined, &is_r_bcrq));
}

#[test]
fn should_reject_single_return_self_recursion_as_impure() {
    // 防递归环（设计 §4.3）：单 RETURN 自递归体虽无 Nop/Return 之外指令，
    // 纯判定与展开都必须以 visiting 环检测拒绝，绝不无限展开。
    let self_rec = compile(
        "CREATE FUNCTION f_self(p_a NUMBER) RETURN NUMBER IS BEGIN RETURN f_self(p_a); END;",
    );
    let cat = catalog_with(&[self_rec]);
    assert!(matches!(summarize(&cat, "f_self"), Summary::Impure));
    assert!(inline_pure_call(&cat, "f_self", &[IrExpr::Const(SpinValue::Int(1))]).is_none());
}

#[test]
fn should_inline_nested_trap_multi_into_with_out_params_as_lookup_chain() {
    // fnc_com_get_seat_para 形态：非常量前缀赋值 + 嵌套 trap 兜底链 +
    // 多目标 INTO（局部变量与 OUT 形参混合）+ 拼接 RETURN。
    // 每个 INTO 目标折叠为计数门控 Lookup；OUT 形参写回表达式随调用返回。
    let f = compile(
        "CREATE FUNCTION f_chain(p_d VARCHAR2, p_f VARCHAR2, p_k VARCHAR2, \
         p_o_flag OUT VARCHAR2, p_o_rate OUT NUMBER) RETURN VARCHAR2 IS \
         v_a VARCHAR2(8); v_b VARCHAR2(8); v_k VARCHAR2(8); \
         BEGIN \
         v_a := ''; v_b := ''; v_k := ''; \
         v_k := substr(p_k, 1, 2) || '00'; \
         BEGIN \
         SELECT t.c1, t.c2, t.c3 INTO v_a, v_b, p_o_flag \
         FROM par_t t WHERE t.f = p_f AND t.k = v_k AND p_d BETWEEN t.d1 AND t.d2; \
         EXCEPTION WHEN OTHERS THEN \
         BEGIN \
         SELECT t.c1, t.c2, t.c3 INTO v_a, v_b, p_o_flag \
         FROM par_t t WHERE t.f = '000000' AND t.k = v_k; \
         EXCEPTION WHEN OTHERS THEN \
         v_a := '1'; v_b := '2'; p_o_flag := '0'; \
         END; \
         END; \
         RETURN v_a || p_o_flag; END;",
    );
    let cat = catalog_with(&[f]);
    assert!(
        matches!(summarize(&cat, "f_chain"), Summary::PureExpr(_)),
        "嵌套 trap 多 INTO + OUT 形态须折叠为 PureExpr"
    );
    let args = [
        IrExpr::Field {
            rec: VarName("r".into()),
            field: "bcrq".into(),
        },
        IrExpr::Field {
            rec: VarName("r".into()),
            field: "fund_code".into(),
        },
        IrExpr::Field {
            rec: VarName("r".into()),
            field: "stock_kind".into(),
        },
        IrExpr::Var(VarName("v_flag".into())),
        IrExpr::Var(VarName("v_rate".into())),
    ];
    let call = inline_pure_call(&cat, "f_chain", &args).expect("inline chain");
    // 返回值 = v_a || p_o_flag：两侧都须是 Lookup（拼接两级兜底链）。
    let IrExpr::Bin {
        op: BinOp::Concat,
        l,
        r,
    } = &call.expr
    else {
        panic!("返回值须为拼接: {:?}", call.expr);
    };
    assert!(matches!(l.as_ref(), IrExpr::Lookup { .. }), "左项须 Lookup");
    assert!(matches!(r.as_ref(), IrExpr::Lookup { .. }), "右项须 Lookup");
    // OUT 写回：p_o_flag（下标 3）表达式为 Lookup，p_o_rate（下标 4）体内未写 → 无条目。
    assert_eq!(call.outs.len(), 1, "仅 p_o_flag 被写回: {:?}", call.outs);
    let (idx, oexpr) = &call.outs[0];
    assert_eq!(*idx, 3);
    let IrExpr::Lookup { query, default, .. } = oexpr else {
        panic!("OUT 写回须为 Lookup: {oexpr:?}");
    };
    // 兜底链：外层默认值本身是内层 Lookup（内层 query 含 '000000' 兜底谓词）。
    let IrExpr::Lookup {
        query: inner_q,
        default: inner_d,
        ..
    } = default.as_ref()
    else {
        panic!("外层 Lookup 默认值须为内层 Lookup: {default:?}");
    };
    assert!(inner_q.contains("'000000'"), "内层须为兜底查询: {inner_q}");
    assert!(
        matches!(inner_d.as_ref(), IrExpr::Const(_)),
        "最内兜底须常量"
    );
    assert!(query.contains("par_t"), "外层 query 须含维表: {query}");
}

#[test]
fn should_reject_handler_write_to_non_into_target() {
    // handler 段对非 INTO 目标的赋值是条件写回，env 无法表达 → 拒绝。
    let f = compile(
        "CREATE FUNCTION f_bad(p_x VARCHAR2) RETURN VARCHAR2 IS \
         v_a VARCHAR2(8); v_b VARCHAR2(8); \
         BEGIN \
         v_a := ''; v_b := ''; \
         BEGIN \
         SELECT t.c1 INTO v_a FROM par_t t WHERE t.f = p_x; \
         EXCEPTION WHEN OTHERS THEN \
         v_a := '1'; v_b := '2'; \
         END; \
         RETURN v_a; END;",
    );
    let cat = catalog_with(&[f]);
    assert!(
        matches!(summarize(&cat, "f_bad"), Summary::Impure),
        "handler 写非 INTO 目标须拒绝"
    );
}
