//! 运行帧：变量作用域 + PL 表达式求值（三值逻辑，NULL 传播）。

use crate::catalog::RoutineCatalog;
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use crate::ir::{AssignTarget, BinOp, IrExpr, VarName};
use crate::runtime::RunOutcome;
use crate::value::SpinValue;
use std::collections::BTreeMap;

use super::cursor::{CursorAttrKind, CursorState};

/// 执行帧（单过程实例；PACKAGE 级状态在 M7 引入 PackageState）。
#[derive(Debug, Default)]
pub struct Frame {
    vars: BTreeMap<String, SpinValue>,
    /// 游标状态表（过程局部，§7.3）
    cursors: BTreeMap<String, CursorState>,
}

impl Frame {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, name: &VarName) -> Option<&SpinValue> {
        self.vars.get(&name.0)
    }
    pub fn set(&mut self, name: &VarName, v: SpinValue) {
        self.vars.insert(name.0.clone(), v);
    }
    pub fn record_field(&self, rec: &VarName, field: &str) -> Result<&SpinValue, EngineError> {
        match self.vars.get(&rec.0) {
            Some(SpinValue::Record(fields)) => fields
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(field))
                .map(|(_, v)| v)
                .ok_or_else(|| {
                    EngineError::business(120, format!("record {} has no field {}", rec.0, field))
                }),
            Some(_) => Err(EngineError::business(
                121,
                format!("{} is not a record", rec.0),
            )),
            None => Err(EngineError::business(
                122,
                format!("undefined record variable {}", rec.0),
            )),
        }
    }
    pub fn set_record_field(
        &mut self,
        rec: &VarName,
        field: &str,
        v: SpinValue,
    ) -> Result<(), EngineError> {
        match self.vars.get_mut(&rec.0) {
            Some(SpinValue::Record(fields)) => {
                match fields
                    .iter_mut()
                    .find(|(n, _)| n.eq_ignore_ascii_case(field))
                {
                    Some(slot) => slot.1 = v,
                    None => fields.push((field.to_string(), v)),
                }
                Ok(())
            }
            Some(_) => Err(EngineError::business(
                121,
                format!("{} is not a record", rec.0),
            )),
            None => Err(EngineError::business(
                122,
                format!("undefined record variable {}", rec.0),
            )),
        }
    }
    /// FETCH INTO 的 %ROWTYPE 目标：未定义（或非 Record）时按结果 schema 建 RECORD
    /// （设计 §7.3.2：字段名取查询别名）。
    pub fn ensure_record(&mut self, rec: &VarName, schema: &[String]) {
        let need = match self.vars.get(&rec.0) {
            Some(SpinValue::Record(fields)) => fields.len() != schema.len(),
            _ => true,
        };
        if need {
            self.vars.insert(
                rec.0.clone(),
                SpinValue::Record(
                    schema
                        .iter()
                        .map(|c| (c.clone(), SpinValue::Null))
                        .collect(),
                ),
            );
        }
    }

    pub fn assign(&mut self, target: &AssignTarget, v: SpinValue) -> Result<(), EngineError> {
        match target {
            AssignTarget::Var(name) => {
                self.set(name, v);
                Ok(())
            }
            AssignTarget::Field { rec, field } => self.set_record_field(rec, field, v),
        }
    }
    pub fn var_names(&self) -> Vec<String> {
        self.vars.keys().cloned().collect()
    }

    /// OPEN：挂载游标状态（快照语义，§7.3.1）。
    pub fn open_cursor(&mut self, name: &VarName, state: CursorState) {
        self.cursors.insert(name.0.to_ascii_lowercase(), state);
    }

    /// FETCH：取游标状态可变引用。
    pub fn cursor_mut(&mut self, name: &VarName) -> Result<&mut CursorState, EngineError> {
        self.cursors
            .get_mut(&name.0.to_ascii_lowercase())
            .ok_or_else(|| EngineError::business(140, format!("cursor {} is not open", name.0)))
    }

    /// 游标 schema 副本（FETCH INTO %ROWTYPE 预建 RECORD 用）。
    pub fn cursor_schema(&self, name: &VarName) -> Vec<String> {
        match self.cursors.get(&name.0.to_ascii_lowercase()) {
            Some(CursorState::Open { stream, .. }) => stream.schema.clone(),
            _ => Vec::new(),
        }
    }

    /// CLOSE。
    pub fn close_cursor(&mut self, name: &VarName) -> Result<(), EngineError> {
        match self.cursors.get_mut(&name.0.to_ascii_lowercase()) {
            Some(cs) => {
                *cs = CursorState::Closed;
                Ok(())
            }
            None => Err(EngineError::business(
                141,
                format!("cursor {} is not open", name.0),
            )),
        }
    }

    /// 游标属性求值（§7.3.3）。
    pub fn cursor_attr(&self, name: &VarName, kind: CursorAttrKind) -> SpinValue {
        match self.cursors.get(&name.0.to_ascii_lowercase()) {
            Some(cs) => cs.attr(kind),
            None => match kind {
                // 未 OPEN 的游标：%ISOPEN=false，%FOUND/%NOTFOUND=false，%ROWCOUNT=0
                CursorAttrKind::IsOpen => SpinValue::Bool(false),
                CursorAttrKind::NotFound | CursorAttrKind::Found => SpinValue::Bool(false),
                CursorAttrKind::RowCount => SpinValue::Int(0),
            },
        }
    }
}

/// 表达式求值环境：PL 上下文函数直调需要例程目录与执行器（§8 分派矩阵）。
pub struct EvalEnv<'a> {
    pub frame: &'a Frame,
    pub ex: &'a dyn SqlExecutor,
    pub cat: &'a RoutineCatalog,
    pub depth: usize,
}

/// 三值逻辑条件求值：Some(bool) 确定性；None = UNKNOWN（EXIT WHEN 按 false）。
pub fn eval_cond(e: &IrExpr, env: &EvalEnv<'_>) -> Result<Option<bool>, EngineError> {
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
pub fn eval(e: &IrExpr, env: &EvalEnv<'_>) -> Result<SpinValue, EngineError> {
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
        IrExpr::Bin { op, l, r } => eval_bin(*op, &eval(l, env)?, &eval(r, env)?),
        IrExpr::CursorAttr { cursor, kind } => Ok(f.cursor_attr(cursor, *kind)),
        IrExpr::SqlFn { name, args } => eval_sqlfn(name, args, env),
        IrExpr::FuncCall { name, args } => eval_funccall(name, args, env),
    }
}

/// §8.2 @变换标记：整体下沉单条 SQL（保留日期等中间类型，不经 SpinValue）。
fn eval_sqlfn(name: &str, args: &[IrExpr], env: &EvalEnv<'_>) -> Result<SpinValue, EngineError> {
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
fn eval_funccall(name: &str, args: &[IrExpr], env: &EvalEnv<'_>) -> Result<SpinValue, EngineError> {
    let Some(routine) = env.cat.lookup(name).cloned() else {
        return eval_builtin(env, name, args);
    };
    if routine.kind != crate::catalog::RoutineKind::Function {
        return Err(EngineError::business(
            153,
            format!("{name} is a procedure and cannot be used in an expression"),
        ));
    }
    if args.len() != routine.params.len() {
        return Err(EngineError::business(
            151,
            format!(
                "{} expects {} args, got {}",
                name,
                routine.params.len(),
                args.len()
            ),
        ));
    }
    if env.depth > 100 {
        return Err(EngineError::invariant(63, "call depth exceeded (100)"));
    }
    let mut callee = Frame::new();
    for (p, ir_a) in routine.params.iter().zip(args) {
        let v = eval(ir_a, env)?;
        callee.set(&VarName(p.name.to_ascii_lowercase()), v);
    }
    let out = crate::runtime::run_with_catalog(
        &routine.prog,
        &mut callee,
        env.ex,
        &mut crate::exec::TraceNoopSink,
        env.cat,
        env.depth + 1,
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

fn eval_builtin(env: &EvalEnv, name: &str, args: &[IrExpr]) -> Result<SpinValue, EngineError> {
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
            | BinOp::Concat
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

fn arith(op: &BinOp, l: &SpinValue, r: &SpinValue) -> Result<SpinValue, EngineError> {
    use SpinValue as V;
    if let (V::Int(x), V::Int(y)) = (l, r) {
        return apply_i(op, *x, *y).map(V::Int);
    }
    // 字符串数值解析为真实 (unscaled, scale)——不得虚拟定标（回归守护：'1.5'+1）
    let (a, sa) = num_parts(l)?;
    let (b, sb) = num_parts(r)?;
    // 统一溢出出口：中间量超 i128 视为数值溢出（显式报错，不静默截断）
    let overflow = || EngineError::business(112, "numeric overflow");
    let (out, scale) = match *op {
        BinOp::Add | BinOp::Sub => {
            let scale = sa.max(sb);
            let a2 = pow10(scale - sa)
                .and_then(|p| a.checked_mul(p))
                .ok_or_else(overflow)?;
            let b2 = pow10(scale - sb)
                .and_then(|p| b.checked_mul(p))
                .ok_or_else(overflow)?;
            let v = if *op == BinOp::Add {
                a2.checked_add(b2)
            } else {
                a2.checked_sub(b2)
            };
            (v.ok_or_else(overflow)?, scale)
        }
        BinOp::Mul => {
            let scale = sa.saturating_add(sb);
            if scale > 38 {
                return Err(overflow());
            }
            (a.checked_mul(b).ok_or_else(overflow)?, scale)
        }
        BinOp::Div => {
            if b == 0 {
                return Err(EngineError::business(124, "division by zero"));
            }
            // value = (a/10^sa) / (b/10^sb)；以 scale S 表示：
            // u = a·10^(sb+S-sa) / b（指数为负时把 10^|k| 移到分母）
            let scale = sa.max(sb);
            let shift = sb as i32 + scale as i32 - sa as i32;
            let (num, den) = if shift >= 0 {
                let p = pow10(shift as u8).ok_or_else(overflow)?;
                (a.checked_mul(p).ok_or_else(overflow)?, b)
            } else {
                let p = pow10((-shift) as u8).ok_or_else(overflow)?;
                (a, b.checked_mul(p).ok_or_else(overflow)?)
            };
            (num / den, scale)
        }
        _ => return Err(EngineError::invariant(30, "not arithmetic")),
    };
    Ok(V::Decimal(out, 38, scale))
}

/// 10^n（n ≤ 38，i128 上界内）；超出返回 None。
fn pow10(n: u8) -> Option<i128> {
    if n > 38 {
        return None;
    }
    let mut r: i128 = 1;
    for _ in 0..n {
        r = r.checked_mul(10)?;
    }
    Some(r)
}

/// 整数路径：除零与 DECIMAL 路径同报 ZERO_DIVIDE（BIZ-124），不得防御性返 0。
fn apply_i(op: &BinOp, x: i64, y: i64) -> Result<i64, EngineError> {
    // 溢出与 DECIMAL 路径同口径显式报错（BIZ-0112），不得静默环绕
    let overflow = || EngineError::business(112, "integer overflow");
    match op {
        BinOp::Add => x.checked_add(y).ok_or_else(overflow),
        BinOp::Sub => x.checked_sub(y).ok_or_else(overflow),
        BinOp::Mul => x.checked_mul(y).ok_or_else(overflow),
        BinOp::Div if y == 0 => Err(EngineError::business(124, "division by zero")),
        BinOp::Div => Ok(x / y),
        _ => Err(EngineError::invariant(30, "not arithmetic")),
    }
}

/// 数值部件：(unscaled, scale)。字符串按十进制字面量解析出真实小数位；
/// 非十进制字面量（科学计数法等）显式拒绝（BIZ-112），不做虚拟定标。
fn num_parts(v: &SpinValue) -> Result<(i128, u8), EngineError> {
    match v {
        SpinValue::Int(i) => Ok((*i as i128, 0)),
        SpinValue::Decimal(v, _, s) => Ok((*v, *s)),
        SpinValue::Str(s) => parse_num_str(s),
        other => Err(EngineError::business(
            125,
            format!("arithmetic requires numeric, got {}", other.display()),
        )),
    }
}

fn parse_num_str(s: &str) -> Result<(i128, u8), EngineError> {
    let t = s.trim();
    let neg = t.starts_with('-');
    let body = t.strip_prefix(['-', '+']).unwrap_or(t);
    let (int_part, frac_part) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    let digits_ok = |x: &str| !x.is_empty() || !frac_part.is_empty();
    let all_ascii_digits = int_part.chars().all(|c| c.is_ascii_digit())
        && frac_part.chars().all(|c| c.is_ascii_digit());
    if !digits_ok(int_part) || !all_ascii_digits {
        return Err(EngineError::business(
            112,
            format!("invalid numeric literal: {s}"),
        ));
    }
    let scale = frac_part.len().min(u8::MAX as usize) as u8;
    let mut digits = format!("{int_part}{frac_part}");
    if digits.is_empty() {
        digits.push('0');
    }
    if digits.len() > 30 {
        return Err(EngineError::business(
            112,
            format!("numeric literal out of range: {s}"),
        ));
    }
    let mut unscaled: i128 = digits
        .parse()
        .map_err(|_| EngineError::business(112, format!("invalid numeric literal: {s}")))?;
    if neg {
        unscaled = -unscaled;
    }
    Ok((unscaled, scale))
}

fn compare(op: &BinOp, l: &SpinValue, r: &SpinValue) -> Result<bool, EngineError> {
    use std::cmp::Ordering as O;
    use SpinValue as V;
    let ord = match (l, r) {
        (V::Int(x), V::Int(y)) => x.cmp(y),
        (V::Str(x), V::Str(y)) => x.cmp(y),
        (V::Bool(x), V::Bool(y)) => x.cmp(y),
        _ => {
            // 数值与字符串混合：可解析为数值则按数值比较（真实 scale 对齐）
            if let (Ok((a, sa)), Ok((b, sb))) = (num_parts(l), num_parts(r)) {
                let s = sa.max(sb);
                // 对齐中间量与算术路径同口径显式报错（BIZ-0112），不得环绕或 panic
                let overflow = || EngineError::business(112, "numeric overflow");
                let pa = pow10(s - sa).and_then(|p| a.checked_mul(p));
                let pb = pow10(s - sb).and_then(|p| b.checked_mul(p));
                match (pa, pb) {
                    (Some(la), Some(lb)) => la.cmp(&lb),
                    _ => return Err(overflow()),
                }
            } else {
                l.as_str_loose().cmp(&r.as_str_loose())
            }
        }
    };
    Ok(match op {
        BinOp::Eq => ord == O::Equal,
        BinOp::Ne => ord != O::Equal,
        BinOp::Lt => ord == O::Less,
        BinOp::Le => ord != O::Greater,
        BinOp::Gt => ord == O::Greater,
        BinOp::Ge => ord != O::Less,
        _ => return Err(EngineError::invariant(31, "not comparison")),
    })
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
