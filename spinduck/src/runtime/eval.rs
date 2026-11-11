//! PL 表达式求值（三值逻辑、NULL 传播、内核重入）；自 `frame.rs` 拆分（AGENTS §3）。
//! 数值运算与比较的定标实现见同目录 `eval::numeric`。

mod numeric;

use crate::catalog::RoutineCatalog;
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use crate::ir::{BinOp, IrExpr, VarName};
use crate::runtime::frame::Frame;
use crate::runtime::RunOutcome;
use crate::value::SpinValue;
use numeric::{arith, compare};

/// 表达式求值环境：PL 上下文函数直调需要例程目录与执行器（§8 分派矩阵）。
///
/// `sink` 也在这里：表达式里直调的例程与 CALL 下去的例程共用同一条埋点链，
/// 断在哪一环，审计日志就少哪一环（§11.1 禁止静默丢弃）。
pub struct EvalEnv<'a> {
    pub frame: &'a Frame,
    pub ex: &'a dyn SqlExecutor,
    pub cat: &'a RoutineCatalog,
    pub depth: usize,
    pub sink: &'a mut dyn crate::exec::TraceSink,
}

/// 三值逻辑条件求值：Some(bool) 确定性；None = UNKNOWN（EXIT WHEN 按 false）。
pub fn eval_cond(e: &IrExpr, env: &mut EvalEnv<'_>) -> Result<Option<bool>, EngineError> {
    let _f = env.frame;
    match eval(e, env)? {
        SpinValue::Null => Ok(None),
        SpinValue::Bool(b) => Ok(Some(b)),
        other => Err(EngineError::business(
            123,
            format!("condition must be boolean, got {}", other.display()),
        )),
    }
}

/// PL 表达式求值（NULL 传播 + 三值逻辑 AND/OR）。
pub fn eval(e: &IrExpr, env: &mut EvalEnv<'_>) -> Result<SpinValue, EngineError> {
    let f = env.frame;
    match e {
        IrExpr::Const(v) => Ok(v.clone()),
        IrExpr::Var(name) => f
            .get(name)
            .cloned()
            .ok_or_else(|| EngineError::business(122, format!("undefined variable {}", name.0))),
        IrExpr::Field { rec, field } => f.record_field(rec, field).cloned(),
        IrExpr::IsNull(inner) => {
            let v = eval(inner, env)?;
            Ok(SpinValue::Bool(matches!(v, SpinValue::Null)))
        }
        IrExpr::Not(inner) => match eval(inner, env)? {
            SpinValue::Null => Ok(SpinValue::Null),
            SpinValue::Bool(b) => Ok(SpinValue::Bool(!b)),
            other => Err(EngineError::business(
                123,
                format!("NOT requires boolean, got {}", other.display()),
            )),
        },
        IrExpr::Bin { op, l, r } => {
            let l = eval(l, env)?;
            let r = eval(r, env)?;
            eval_bin(*op, &l, &r)
        }
        IrExpr::CursorAttr { cursor, kind } => Ok(f.cursor_attr(cursor, *kind)),
        IrExpr::SqlFn { name, args } => eval_sqlfn(name, args, env),
        IrExpr::FuncCall { name, args } => eval_funccall(name, args, env),
    }
}

/// §8.2 @变换标记：整体下沉单条 SQL（保留日期等中间类型，不经 SpinValue）。
fn eval_sqlfn(
    name: &str,
    args: &[IrExpr],
    env: &mut EvalEnv<'_>,
) -> Result<SpinValue, EngineError> {
    let Some((arg_sql, binds_vec)) = crate::sql::render::expr_to_sql_binds(&e_sqlfn_args(args))
    else {
        return Err(EngineError::invariant(
            64,
            "SqlFn argument not renderable (cursor attribute?)",
        ));
    };
    let sql = format!("SELECT {}({})", name.to_ascii_lowercase(), arg_sql);
    let sql = crate::sql::apply_ast_transforms(&sql)?;
    let bind_map = crate::runtime::interp::resolve_binds(&binds_vec, env.frame)?;
    let mut rows = env.ex.query(&sql, &bind_map)?;
    Ok(rows
        .fetch_many(1)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .unwrap_or(SpinValue::Null))
}

/// §8 分派矩阵：PL 上下文 → 例程目录直调；未命中视为内建/库函数，
/// 下沉为 `SELECT f($p0,…)` 交给内核求值（内建函数语义由 DuckDB 提供，
/// Gauss 方言差异由编译期规则改名 + 解析级变换承担）。
fn eval_funccall(
    name: &str,
    args: &[IrExpr],
    env: &mut EvalEnv<'_>,
) -> Result<SpinValue, EngineError> {
    let Some(routine) = env.cat.lookup(name).cloned() else {
        return eval_builtin(env, name, args);
    };
    if routine.kind != crate::catalog::RoutineKind::Function {
        return Err(EngineError::business(
            153,
            format!("{name} is a procedure and cannot be used in an expression"),
        ));
    }
    crate::catalog::check_arity(name, &routine.params, args.len())?;
    if env.depth > 100 {
        return Err(EngineError::invariant(63, "call depth exceeded (100)"));
    }
    let mut callee = Frame::new();
    for (i, p) in routine.params.iter().enumerate() {
        let v = match args.get(i) {
            Some(ir_a) => eval(ir_a, env)?,
            None => p.default.clone().unwrap_or(SpinValue::Null),
        };
        callee.set(&VarName(p.name.to_ascii_lowercase()), v);
    }
    let out = crate::runtime::run_with_catalog(
        &routine.prog,
        &mut callee,
        env.ex,
        &mut *env.sink,
        env.cat,
        env.depth + 1,
        crate::runtime::interp::RunLimits::default(),
    )?;
    match out {
        RunOutcome::Returned(Some(v)) => Ok(v),
        _ => Ok(SpinValue::Null),
    }
}

/// PL 上下文的内建/库函数参数：以 SqlFn 形态渲染参数列表（供下沉）。
fn e_sqlfn_args(args: &[IrExpr]) -> IrExpr {
    IrExpr::SqlFn {
        name: String::new(),
        args: args.to_vec(),
    }
}

fn eval_builtin(
    env: &mut EvalEnv<'_>,
    name: &str,
    args: &[IrExpr],
) -> Result<SpinValue, EngineError> {
    let mut binds = crate::exec::BindMap::new();
    let mut ph = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        let key = format!("$p{i}");
        binds.insert(key.clone(), eval(a, env)?);
        ph.push(key);
    }
    let sql = format!("SELECT {}({})", name, ph.join(", "));
    let sql = crate::sql::apply_ast_transforms(&sql)?;
    let mut rows = env.ex.query(&sql, &binds)?;
    Ok(rows
        .fetch_many(1)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .unwrap_or(SpinValue::Null))
}

fn eval_bin(op: BinOp, l: &SpinValue, r: &SpinValue) -> Result<SpinValue, EngineError> {
    use SpinValue as V;
    // NULL 传播：算术/比较任一侧 NULL → NULL；AND/OR 走三值逻辑
    let null_prop = matches!(
        op,
        BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
    );
    if null_prop && (matches!(l, V::Null) || matches!(r, V::Null)) {
        return Ok(V::Null);
    }
    match op {
        // A 模式文本拼接：NULL 侧视作空串，全 NULL 才为 NULL（基线实测
        // `'A' || NULL` = 'A'、`NULL || NULL` = NULL）；SQL 侧同口径改写见
        // sql::rewrite::concat（'a' || b → concat('a', b)）。
        BinOp::Concat if matches!((l, r), (V::Null, V::Null)) => Ok(V::Null),
        BinOp::Concat => Ok(V::Str(format!("{}{}", l.as_str_loose(), r.as_str_loose()))),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arith(&op, l, r),
        BinOp::Eq => Ok(V::Bool(compare(&op, l, r)?)),
        // compare() 内部已按传入运算符求值；Ne 只能对 Eq 结果取反，
        // 否则 `<>` 会被双重取反（回归守护见 tests/ir_expr_forms.rs）
        BinOp::Ne => Ok(V::Bool(!compare(&BinOp::Eq, l, r)?)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(V::Bool(compare(&op, l, r)?)),
        BinOp::And => and3(l, r),
        BinOp::Or => or3(l, r),
    }
}

fn and3(l: &SpinValue, r: &SpinValue) -> Result<SpinValue, EngineError> {
    let lb = bool3(l)?;
    let rb = bool3(r)?;
    Ok(match (lb, rb) {
        (Some(false), _) | (_, Some(false)) => SpinValue::Bool(false),
        (Some(true), Some(true)) => SpinValue::Bool(true),
        _ => SpinValue::Null,
    })
}

fn or3(l: &SpinValue, r: &SpinValue) -> Result<SpinValue, EngineError> {
    let lb = bool3(l)?;
    let rb = bool3(r)?;
    Ok(match (lb, rb) {
        (Some(true), _) | (_, Some(true)) => SpinValue::Bool(true),
        (Some(false), Some(false)) => SpinValue::Bool(false),
        _ => SpinValue::Null,
    })
}

fn bool3(v: &SpinValue) -> Result<Option<bool>, EngineError> {
    match v {
        SpinValue::Null => Ok(None),
        SpinValue::Bool(b) => Ok(Some(*b)),
        other => Err(EngineError::business(
            123,
            format!("logical op requires boolean, got {}", other.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::BinOp;

    fn ev(op: BinOp, l: SpinValue, r: SpinValue) -> SpinValue {
        eval_bin(op, &l, &r).unwrap()
    }

    #[test]
    fn should_add_string_decimal_with_true_scale() {
        // 回归守护：字符串数值曾按虚拟定标 ×10⁶ 参与运算（'1.5'+1 → 1500001）
        assert_eq!(
            ev(BinOp::Add, SpinValue::Str("1.5".into()), SpinValue::Int(1)),
            SpinValue::Decimal(25, 38, 1) // 2.5
        );
        assert_eq!(
            ev(
                BinOp::Mul,
                SpinValue::Str("0.1".into()),
                SpinValue::Str("0.2".into())
            ),
            SpinValue::Decimal(2, 38, 2) // 0.02
        );
    }

    #[test]
    fn should_treat_null_as_empty_string_in_concat_per_a_mode() {
        // 基线实测（openGauss 5.0.0，A 模式）：'A' || NULL = 'A'、NULL || NULL = NULL，
        // 数值参与拼接自动转串（1 || 2 = '12'）
        assert_eq!(
            ev(BinOp::Concat, SpinValue::Str("A".into()), SpinValue::Null),
            SpinValue::Str("A".into())
        );
        assert_eq!(
            ev(BinOp::Concat, SpinValue::Null, SpinValue::Str("B".into())),
            SpinValue::Str("B".into())
        );
        assert_eq!(
            ev(BinOp::Concat, SpinValue::Null, SpinValue::Null),
            SpinValue::Null
        );
        assert_eq!(
            ev(BinOp::Concat, SpinValue::Int(1), SpinValue::Int(2)),
            SpinValue::Str("12".into())
        );
    }

    #[test]
    fn should_compare_mixed_numeric_by_value() {
        // 字符串小数与整数按数值比较，不得按虚拟定标错序
        assert_eq!(
            ev(BinOp::Gt, SpinValue::Str("2.5".into()), SpinValue::Int(2)),
            SpinValue::Bool(true)
        );
        assert_eq!(
            ev(BinOp::Lt, SpinValue::Str("1.5".into()), SpinValue::Int(2)),
            SpinValue::Bool(true)
        );
    }

    #[test]
    fn should_report_zero_divide_on_int_division() {
        // INT 路径除零与 DECIMAL 路径同报 BIZ-124，不得防御性返 0
        let e = eval_bin(BinOp::Div, &SpinValue::Int(5), &SpinValue::Int(0)).unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0124");
    }

    #[test]
    fn should_report_int_overflow_not_wrap() {
        // INT 路径溢出与 DECIMAL 路径同报 BIZ-0112，不得静默环绕
        let big = SpinValue::Int(i64::MAX);
        let e = eval_bin(BinOp::Add, &big, &SpinValue::Int(1)).unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0112");
    }

    #[test]
    fn should_report_overflow_on_wide_scale_compare_not_wrap() {
        // 跨定标对齐（100 vs 1e-38）的中间量超 i128：与算术路径同口径报
        // BIZ-0112，不得 panic（debug 环绕检查）或静默给出错序结果。
        let e = eval_bin(
            BinOp::Lt,
            &SpinValue::Int(100),
            &SpinValue::Decimal(1, 38, 38),
        )
        .unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0112");
        assert!(
            e.message.contains("overflow"),
            "消息须说明溢出，实际: {}",
            e.message
        );
    }

    #[test]
    fn should_reject_non_decimal_numeric_string() {
        // 科学计数法等非十进制字面量显式拒绝（不做虚拟定标）
        let e = eval_bin(
            BinOp::Add,
            &SpinValue::Str("1e3".into()),
            &SpinValue::Int(1),
        )
        .unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0112");
    }

    #[test]
    fn should_evaluate_ne_as_inequality_not_inverted() {
        // 回归：Ne 曾对 compare() 结果二次取反，导致 `<>`/`!=` 恒为等值判断
        assert_eq!(
            ev(
                BinOp::Ne,
                SpinValue::Str("07".into()),
                SpinValue::Str("02".into())
            ),
            SpinValue::Bool(true)
        );
        assert_eq!(
            ev(
                BinOp::Ne,
                SpinValue::Str("02".into()),
                SpinValue::Str("02".into())
            ),
            SpinValue::Bool(false)
        );
        assert_eq!(
            ev(BinOp::Ne, SpinValue::Int(1), SpinValue::Int(2)),
            SpinValue::Bool(true)
        );
        assert_eq!(
            ev(BinOp::Eq, SpinValue::Int(1), SpinValue::Int(1)),
            SpinValue::Bool(true)
        );
    }

    #[test]
    fn should_propagate_null_for_comparison_and_arith() {
        assert_eq!(
            ev(BinOp::Eq, SpinValue::Null, SpinValue::Int(1)),
            SpinValue::Null
        );
        assert_eq!(
            ev(BinOp::Add, SpinValue::Int(1), SpinValue::Null),
            SpinValue::Null
        );
    }

    #[test]
    fn should_apply_three_valued_logic_for_and_or() {
        assert_eq!(
            ev(BinOp::And, SpinValue::Bool(false), SpinValue::Null),
            SpinValue::Bool(false)
        );
        assert_eq!(
            ev(BinOp::And, SpinValue::Bool(true), SpinValue::Null),
            SpinValue::Null
        );
        assert_eq!(
            ev(BinOp::Or, SpinValue::Bool(true), SpinValue::Null),
            SpinValue::Bool(true)
        );
        assert_eq!(
            ev(BinOp::Or, SpinValue::Bool(false), SpinValue::Null),
            SpinValue::Null
        );
    }
}
