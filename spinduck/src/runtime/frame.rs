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
        IrExpr::FuncCall { name, args } => {
            // §8 分派矩阵：PL 上下文 → 解释器直调（含嵌套；只支持函数）
            let routine = env
                .cat
                .lookup(name)
                .ok_or_else(|| EngineError::business(152, format!("undefined function: {name}")))?
                .clone();
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
            let noop = crate::exec::TraceNoopSink;
            let out = crate::runtime::run_with_catalog(
                &routine.prog,
                &mut callee,
                env.ex,
                &mut crate::exec::TraceNoopSink,
                env.cat,
                env.depth + 1,
            )?;
            let _ = noop;
            match out {
                RunOutcome::Returned(Some(v)) => Ok(v),
                RunOutcome::Returned(None) => Ok(SpinValue::Null),
                RunOutcome::Completed => Ok(SpinValue::Null),
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
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
        BinOp::Ne => Ok(V::Bool(!compare(&op, l, r)?)),
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
    let (a, b) = match (l, r) {
        (V::Int(x), V::Int(y)) => return Ok(V::Int(apply_i(op, *x, *y))),
        _ => (num_unscaled(l)?, num_unscaled(r)?),
    };
    let scale = num_scale(l).max(num_scale(r));
    let div = 10i128.pow(scale as u32);
    // 对齐到公共 scale 后做整数运算（DECIMAL 语义）
    let (a, b) = (
        a * (div / 10i128.pow(num_scale(l) as u32)),
        b * (div / 10i128.pow(num_scale(r) as u32)),
    );
    let out = match *op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b / div,
        BinOp::Div => {
            if b == 0 {
                return Err(EngineError::business(124, "division by zero"));
            }
            a * div / b
        }
        _ => return Err(EngineError::invariant(30, "not arithmetic")),
    };
    Ok(V::Decimal(out, 38, scale))
}

fn apply_i(op: &BinOp, x: i64, y: i64) -> i64 {
    match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => {
            if y == 0 {
                0 // 除零在 DECIMAL 路径报错；INT 路径防御性返回 0（探针覆盖）
            } else {
                x / y
            }
        }
        _ => 0,
    }
}

fn num_unscaled(v: &SpinValue) -> Result<i128, EngineError> {
    match v {
        SpinValue::Int(i) => Ok(*i as i128),
        SpinValue::Decimal(v, _, _) => Ok(*v),
        SpinValue::Str(s) => {
            let t = s.trim();
            t.parse::<i128>()
                .or_else(|_| t.parse::<f64>().map(|f| (f * 1_000_000.0).round() as i128))
                .map_err(|_| EngineError::business(112, format!("invalid numeric literal: {s}")))
        }
        other => Err(EngineError::business(
            125,
            format!("arithmetic requires numeric, got {}", other.display()),
        )),
    }
}

fn num_scale(v: &SpinValue) -> u8 {
    match v {
        SpinValue::Decimal(_, _, s) => *s,
        _ => 0,
    }
}

fn compare(op: &BinOp, l: &SpinValue, r: &SpinValue) -> Result<bool, EngineError> {
    use std::cmp::Ordering as O;
    use SpinValue as V;
    let ord = match (l, r) {
        (V::Int(x), V::Int(y)) => x.cmp(y),
        (V::Str(x), V::Str(y)) => x.cmp(y),
        (V::Bool(x), V::Bool(y)) => x.cmp(y),
        _ => {
            // 数值与字符串混合：按宽松字符串比较（GaussDB VARCHAR2 语义近似）
            let (a, b) = (num_of(l), num_of(r));
            if let (Some(a), Some(b)) = (a, b) {
                let s = num_scale(l).max(num_scale(r));
                let div = 10i128.pow(s as u32);
                let (la, lb) = (
                    a * (div / 10i128.pow(num_scale(l) as u32)),
                    b * (div / 10i128.pow(num_scale(r) as u32)),
                );
                la.cmp(&lb)
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

fn num_of(v: &SpinValue) -> Option<i128> {
    match v {
        SpinValue::Int(i) => Some(*i as i128),
        SpinValue::Decimal(v, _, _) => Some(*v),
        SpinValue::Str(s) => s.trim().parse::<i64>().ok().map(|i| i as i128),
        _ => None,
    }
}
