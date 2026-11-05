//! 数值定标与比较（DECIMAL 私有量纲 i128×10^-scale；AGENTS §5 溢出显式报错）。

use crate::error::EngineError;
use crate::ir::BinOp;
use crate::value::SpinValue;

pub(super) fn arith(op: &BinOp, l: &SpinValue, r: &SpinValue) -> Result<SpinValue, EngineError> {
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
pub(super) fn pow10(n: u8) -> Option<i128> {
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
pub(super) fn apply_i(op: &BinOp, x: i64, y: i64) -> Result<i64, EngineError> {
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
pub(super) fn num_parts(v: &SpinValue) -> Result<(i128, u8), EngineError> {
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

pub(super) fn parse_num_str(s: &str) -> Result<(i128, u8), EngineError> {
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

pub(super) fn compare(op: &BinOp, l: &SpinValue, r: &SpinValue) -> Result<bool, EngineError> {
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
