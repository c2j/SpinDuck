//! 运行帧：变量作用域 + PL 表达式求值（三值逻辑，NULL 传播）。

use crate::error::EngineError;
use crate::ir::{AssignTarget, BinOp, IrExpr, VarName};
use crate::value::SpinValue;
use std::collections::BTreeMap;

/// 执行帧（单过程实例；PACKAGE 级状态在 M7 引入 PackageState）。
#[derive(Debug, Default, Clone)]
pub struct Frame {
    vars: BTreeMap<String, SpinValue>,
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
}

/// 三值逻辑条件求值：Some(bool) 确定性；None = UNKNOWN（EXIT WHEN 按 false）。
pub fn eval_cond(e: &IrExpr, f: &Frame) -> Result<Option<bool>, EngineError> {
    match eval(e, f)? {
        SpinValue::Null => Ok(None),
        SpinValue::Bool(b) => Ok(Some(b)),
        other => Err(EngineError::business(
            123,
            format!("condition must be boolean, got {}", other.display()),
        )),
    }
}

/// PL 表达式求值（NULL 传播 + 三值逻辑 AND/OR）。
pub fn eval(e: &IrExpr, f: &Frame) -> Result<SpinValue, EngineError> {
    match e {
        IrExpr::Const(v) => Ok(v.clone()),
        IrExpr::Var(name) => f
            .get(name)
            .cloned()
            .ok_or_else(|| EngineError::business(122, format!("undefined variable {}", name.0))),
        IrExpr::Field { rec, field } => f.record_field(rec, field).cloned(),
        IrExpr::IsNull(inner) => {
            let v = eval(inner, f)?;
            Ok(SpinValue::Bool(matches!(v, SpinValue::Null)))
        }
        IrExpr::Not(inner) => match eval(inner, f)? {
            SpinValue::Null => Ok(SpinValue::Null),
            SpinValue::Bool(b) => Ok(SpinValue::Bool(!b)),
            other => Err(EngineError::business(
                123,
                format!("NOT requires boolean, got {}", other.display()),
            )),
        },
        IrExpr::Bin { op, l, r } => eval_bin(*op, &eval(l, f)?, &eval(r, f)?),
        IrExpr::FuncCall { name, .. } => Err(EngineError::invariant(
            29,
            format!("routine dispatch not yet available (M6): {name}"),
        )),
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
