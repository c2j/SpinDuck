//! E2 确定性调用物化的静态证明测试（`callmat`）。
//!
//! 锁定行为：`FuncCall`/`SqlFn` 仅当 callee 例程体可物化（无写/事务/
//! 游标/动态/Raise）或内建 `prepared` 就绪时才进入 `G_pure`；
//! 目录 `Procedure`、含写例程、未备 `prepared` 一律交权。

use super::analyze;
use super::callmat::{expr_materializable, routine_materializable, stmt_exprs_materializable};
use crate::catalog::{CompiledRoutine, ParamMode, RoutineCatalog, RoutineKind, RoutineParam};
use crate::ir::{AssignTarget, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, ScalarSql, VarName};
use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;
use std::collections::BTreeMap;

fn vn(s: &str) -> VarName {
    VarName(s.into())
}

fn e_int(n: i64) -> IrExpr {
    IrExpr::Const(SpinValue::Int(n))
}

fn assign_var(target: &str, expr: IrExpr) -> IrStmt {
    IrStmt::Assign {
        target: AssignTarget::Var(vn(target)),
        expr,
    }
}

fn func_call(name: &str, args: Vec<IrExpr>) -> IrExpr {
    let ph: Vec<String> = (0..args.len()).map(|i| format!("$p{i}")).collect();
    IrExpr::FuncCall {
        name: name.into(),
        args,
        prepared: Ok(format!("SELECT {name}({})", ph.join(", "))),
    }
}

fn sql_exec(sql: &str) -> IrStmt {
    IrStmt::SqlExec {
        sql: sql.into(),
        binds: Vec::new(),
        fn_sites: Vec::new(),
        buffer_policy: crate::ir::BufferPolicy::Immediate,
    }
}

fn catalog_with(name: &str, kind: RoutineKind, body: Vec<IrStmt>) -> RoutineCatalog {
    let mut catalog = RoutineCatalog::new();
    catalog.register(CompiledRoutine {
        name: name.into(),
        kind,
        params: vec![RoutineParam {
            name: "p0".into(),
            mode: ParamMode::In,
            default: None,
            ty: None,
        }],
        prog: IrProg {
            code: body,
            var_types: BTreeMap::new(),
            local_ddl: Vec::new(),
        },
    });
    catalog
}

/// 纯计算例程：赋值 + 返回。
fn pure_fn_body() -> Vec<IrStmt> {
    vec![
        assign_var("v_x", e_int(1)),
        IrStmt::Return {
            value: Some(e_int(2)),
        },
    ]
}

#[test]
fn should_admit_catalog_function_with_pure_body() {
    let catalog = catalog_with("pkg.f", RoutineKind::Function, pure_fn_body());
    let expr = func_call("pkg.f", vec![e_int(1)]);
    assert!(expr_materializable(&expr, &catalog, 0));
}

#[test]
fn should_reject_function_whose_body_writes() {
    let mut body = pure_fn_body();
    body.insert(0, sql_exec("INSERT INTO t VALUES (1)"));
    let catalog = catalog_with("pkg.f", RoutineKind::Function, body);
    let expr = func_call("pkg.f", vec![e_int(1)]);
    assert!(!expr_materializable(&expr, &catalog, 0));
}

#[test]
fn should_reject_function_whose_body_opens_cursor() {
    let mut body = pure_fn_body();
    body.insert(
        0,
        IrStmt::OpenCursor {
            cursor: vn("c2"),
            source: CursorSource::Static {
                sql: "SELECT 1".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            },
        },
    );
    let catalog = catalog_with("pkg.f", RoutineKind::Function, body);
    let expr = func_call("pkg.f", vec![e_int(1)]);
    assert!(!expr_materializable(&expr, &catalog, 0));
}

#[test]
fn should_reject_procedure_kind_in_expression_position() {
    let catalog = catalog_with("pkg.p", RoutineKind::Procedure, pure_fn_body());
    let expr = func_call("pkg.p", vec![e_int(1)]);
    assert!(!expr_materializable(&expr, &catalog, 0));
}

#[test]
fn should_admit_builtin_funccall_when_prepared_ready() {
    let catalog = RoutineCatalog::new();
    let expr = func_call("unknown_fn", vec![e_int(1)]);
    assert!(expr_materializable(&expr, &catalog, 0));
}

#[test]
fn should_reject_funccall_when_prepared_failed() {
    let catalog = RoutineCatalog::new();
    let mut expr = func_call("unknown_fn", vec![e_int(1)]);
    if let IrExpr::FuncCall { prepared, .. } = &mut expr {
        *prepared = Err(crate::error::EngineError::invariant(1, "no sql"));
    }
    assert!(!expr_materializable(&expr, &catalog, 0));
}

#[test]
fn should_admit_sqlfn_when_prepared_ready() {
    let catalog = RoutineCatalog::new();
    let expr = IrExpr::SqlFn {
        name: "to_char".into(),
        args: vec![e_int(1)],
        prepared: Ok(ScalarSql {
            sql: "SELECT to_char($p0)".into(),
            binds: vec![("$p0".into(), BindSrc::Var(vn("v_a")))],
        }),
    };
    assert!(expr_materializable(&expr, &catalog, 0));
}

#[test]
fn should_reject_lookup_expr() {
    let catalog = RoutineCatalog::new();
    let expr = IrExpr::Lookup {
        query: "SELECT 1".into(),
        args: vec![],
        default: Box::new(e_int(0)),
    };
    assert!(!expr_materializable(&expr, &catalog, 0));
}

// ---------------------------------------------------------------------------
// analyze 级：物化调用语句进入 G_pure（交权点后移）
// ---------------------------------------------------------------------------

fn cursor_prog(body: Vec<IrStmt>, suffix: Vec<IrStmt>) -> IrProg {
    let cursor = vn("c");
    let mut code = vec![
        IrStmt::OpenCursor {
            cursor: cursor.clone(),
            source: CursorSource::Static {
                sql: "SELECT a FROM src".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            },
        },
        IrStmt::Fetch {
            cursor: cursor.clone(),
            targets: vec![AssignTarget::Var(vn("rec"))],
            record: true,
        },
        IrStmt::Nop,
    ];
    let fetch_pc = 1usize;
    code.extend(body);
    code.push(IrStmt::Jump(fetch_pc));
    let close_pc = code.len();
    code.push(IrStmt::CloseCursor {
        cursor: cursor.clone(),
    });
    code[2] = IrStmt::JumpIf {
        cond: IrExpr::CursorAttr {
            cursor,
            kind: CursorAttrKind::NotFound,
        },
        to: close_pc + 1,
    };
    code.extend(suffix);
    IrProg {
        code,
        var_types: BTreeMap::new(),
        local_ddl: Vec::new(),
    }
}

/// 循环体：4 条纯赋值垫底工作量 + 1 条物化 FuncCall 赋值。
/// 函数体可物化 → 该 Assign 应进入 `g_pure`；不可物化时它是
/// 交权边界（仍计入 compute 与否取决于前缀可达性）。
fn body_with_call(call_expr: IrExpr) -> Vec<IrStmt> {
    vec![
        assign_var("v_a", e_int(1)),
        assign_var("v_b", e_int(2)),
        assign_var("v_c", e_int(3)),
        assign_var("v_d", e_int(4)),
        assign_var("v_r", call_expr),
    ]
}

#[test]
fn should_include_materializable_call_assign_in_g_pure() {
    let catalog = catalog_with("pkg.f", RoutineKind::Function, pure_fn_body());
    let prog = cursor_prog(
        body_with_call(func_call("pkg.f", vec![e_int(1)])),
        vec![IrStmt::Return { value: None }],
    );
    let plan = analyze(&prog, &catalog, false).expect("应通过静态门禁");
    // body_start_pc = fetch_pc+2 = 3；前 4 条赋值 pc 3..6，call 赋值 pc 7。
    assert!(
        plan.g_pure.contains(&7),
        "可物化 FuncCall 赋值应进入 G_pure，实际 g_pure={:?}",
        plan.g_pure
    );
}

#[test]
fn should_exclude_impure_call_assign_from_g_pure() {
    let mut body = pure_fn_body();
    body.insert(0, sql_exec("INSERT INTO t VALUES (1)"));
    let catalog = catalog_with("pkg.f", RoutineKind::Function, body);
    let prog = cursor_prog(
        body_with_call(func_call("pkg.f", vec![e_int(1)])),
        vec![IrStmt::Return { value: None }],
    );
    let plan = analyze(&prog, &catalog, false).expect("交权不拒绝");
    assert!(!plan.g_pure.contains(&7), "含写例程的 FuncCall 必须交权");
}

/// 嵌套调用：外层纯、内层含写 → 整体不可物化。
#[test]
fn should_reject_nested_call_to_impure_callee() {
    let mut inner_body = pure_fn_body();
    inner_body.insert(0, sql_exec("DELETE FROM t"));
    let mut catalog = catalog_with("pkg.inner", RoutineKind::Function, inner_body);
    catalog.register(CompiledRoutine {
        name: "pkg.outer".into(),
        kind: RoutineKind::Function,
        params: vec![],
        prog: IrProg {
            code: vec![
                assign_var("v_y", func_call("pkg.inner", vec![e_int(1)])),
                IrStmt::Return { value: None },
            ],
            var_types: BTreeMap::new(),
            local_ddl: Vec::new(),
        },
    });
    let expr = func_call("pkg.outer", vec![e_int(1)]);
    assert!(!expr_materializable(&expr, &catalog, 0));
    // routine_materializable 直接入口同样拒绝。
    let outer = catalog.lookup("pkg.outer").expect("已注册");
    assert!(!routine_materializable(&outer.prog, &catalog, 0));
    // stmt_exprs_materializable 对含调用的 Assign 同步拒绝。
    let stmt = assign_var("v_z", func_call("pkg.outer", vec![e_int(1)]));
    assert!(!stmt_exprs_materializable(&stmt, &catalog, 0));
}
