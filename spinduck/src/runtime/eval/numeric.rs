//! 数值定标与比较（DECIMAL 私有量纲 i128×10^-scale；AGENTS §5 溢出显式报错）。

use crate::error::EngineError;
use crate::ir::BinOp;
use crate::value::SpinValue;

pub(super) fn arith(op: &BinOp, l: &SpinValue, r: &SpinValue) -> Result<SpinValue, EngineError> {
    use SpinValue as V;
    if let (V::Int(x), V::Int(y)) = (l, r) {
        // GaussDB A 模式：整数 `/` 按 float8 计算（15 位有效数字），其余
        // 整数运算仍是 i64 精确语义（2026-11-28 openGauss 5.0.0 实测）。
        if *op == BinOp::Div {
            return div_int_as_float8(*x, *y);
        }
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
        BinOp::Mod => {
            // GaussDB A 模式（Oracle 语义）实测：小数位取操作数较大者
            // （7.5 % 2 = 1.5），符号随被除数（Rust rem 同口径）；
            // MOD(n, 0) = n——零模返回对齐定标后的被除数，不报除零
            let scale = sa.max(sb);
            let a2 = pow10(scale - sa)
                .and_then(|p| a.checked_mul(p))
                .ok_or_else(overflow)?;
            if b == 0 {
                return Ok(V::Decimal(a2, 38, scale));
            }
            let b2 = pow10(scale - sb)
                .and_then(|p| b.checked_mul(p))
                .ok_or_else(overflow)?;
            (a2.checked_rem(b2).ok_or_else(overflow)?, scale)
        }
        BinOp::Div => return div_dec(a, sa, b, sb),
        _ => return Err(EngineError::invariant(30, "not arithmetic")),
    };
    Ok(V::Decimal(out, 38, scale))
}

/// DECIMAL 除法：结果小数位按 PostgreSQL `select_div_scale`（与 GaussDB
/// A 模式 numeric/numeric 同源），商按 half-away-from-zero 舍入。
/// 除零与整数路径同报 BIZ-124。
fn div_dec(a: i128, sa: u8, b: i128, sb: u8) -> Result<SpinValue, EngineError> {
    let overflow = || EngineError::business(112, "numeric overflow");
    if b == 0 {
        return Err(EngineError::business(124, "division by zero"));
    }
    let scale = select_div_scale(a, sa, b, sb)?;
    // value = (a/10^sa) / (b/10^sb)；以 scale S 表示：
    // u = a·10^(sb+S-sa) / b（指数为负时把 10^|k| 移到分母）
    let shift = sb as i32 + scale as i32 - sa as i32;
    let (num, den) = if shift >= 0 {
        let p = pow10(shift as u8).ok_or_else(overflow)?;
        (a.checked_mul(p).ok_or_else(overflow)?, b)
    } else {
        let p = pow10((-shift) as u8).ok_or_else(overflow)?;
        (a, b.checked_mul(p).ok_or_else(overflow)?)
    };
    let q = div_round_half_away(num, den)?;
    Ok(SpinValue::Decimal(q, 38, scale))
}

/// `select_div_scale`（numeric.c）：商的小数位先保证 `NUMERIC_MIN_SIG_DIGITS=16`
/// 位有效数字（`16 - qweight×DEC_DIGITS=4`），再不得小于任一操作数的显示
/// 小数位；PG 上限 1000，本仓 i128 定标上界 38，超出显式报错（不得静默截断）。
fn select_div_scale(a: i128, sa: u8, b: i128, sb: u8) -> Result<u8, EngineError> {
    let (w1, f1) = weight_first_digit(a, sa);
    let (w2, f2) = weight_first_digit(b, sb);
    // 首位数字相等时无法断定大小，PG 按商降一档处理
    let mut qweight = w1 - w2;
    if f1 <= f2 {
        qweight -= 1;
    }
    let rscale = (16 - qweight * 4).max(sa as i32).max(sb as i32).max(0);
    if rscale > 38 {
        return Err(EngineError::business(112, "numeric overflow"));
    }
    Ok(rscale as u8)
}

/// 操作数在基 10000 下的（权重, 首位数字），即 numeric.c 里归一化后的
/// `weight` / `firstdigit`：value = a×10^-scale，十进制首权 dec = 位数-1-scale，
/// base-10000 权 = dec 向下取整除 4，首位组 = dec mod 4 + 1 位有效数字。
fn weight_first_digit(a: i128, scale: u8) -> (i32, u32) {
    if a == 0 {
        return (0, 0);
    }
    let n = a.unsigned_abs();
    let mut digits = 0u32;
    let mut t = n;
    while t > 0 {
        digits += 1;
        t /= 10;
    }
    let dec = digits as i32 - 1 - scale as i32;
    let weight = dec.div_euclid(4);
    let group = dec.rem_euclid(4) + 1;
    let group = group as u32;
    let first = if group <= digits {
        n / 10u128.pow(digits - group)
    } else {
        // 纯小数有效位不足首位组：右侧补零（0.00001 → 首位组 1000）
        n * 10u128.pow(group - digits)
    };
    (weight, first as u32)
}

/// 商按 half-away-from-zero 舍入（div_var 的 round 口径：余数×2 ≥ 除数则进位）。
fn div_round_half_away(num: i128, den: i128) -> Result<i128, EngineError> {
    let neg = (num < 0) != (den < 0);
    let n = num.unsigned_abs();
    let d = den.unsigned_abs();
    let mut q = n / d;
    // 用 u128 比较 `2r ≥ d`，避免 r×2 溢出
    if n % d >= d - (n % d) {
        q += 1;
    }
    let q = i128::try_from(q).map_err(|_| EngineError::business(112, "numeric overflow"))?;
    Ok(if neg { -q } else { q })
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

/// 整数除法按 GaussDB A 模式的 float8 语义求值：先以 f64 计算，再按
/// `DBL_DIG=15` 位有效数字固化为 DECIMAL（`1/2` → 0.5、
/// `9007199254740993/1` → 9007199254740990）。除零与 DECIMAL 路径同报
/// BIZ-124，不得静默返回 0 或 inf。
fn div_int_as_float8(x: i64, y: i64) -> Result<SpinValue, EngineError> {
    if y == 0 {
        return Err(EngineError::business(124, "division by zero"));
    }
    // `{:.14e}` = 1 位整数 + 14 位小数 = 15 位有效数字，与 C
    // `printf("%.15g")` / float8_numeric 的舍入口径一致。
    let text = format!("{:.14e}", x as f64 / y as f64);
    decimal_from_sig15(&text)
}

/// 把 `[-]d.dddddddddddddde±N`（15 位有效数字）解析成真实定标的 DECIMAL；
/// 去掉尾零以复现 float8 → NUMBER 的规范形态（0.5 而非 0.500000000000000）。
fn decimal_from_sig15(text: &str) -> Result<SpinValue, EngineError> {
    let invalid = || EngineError::invariant(30, "invalid float8 decimal");
    let (mantissa, exp) = text.split_once('e').ok_or_else(invalid)?;
    let exp: i32 = exp.parse().map_err(|_| invalid())?;
    let neg = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Err(invalid());
    }
    let mut unscaled: i128 = digits.parse().map_err(|_| invalid())?;
    if neg {
        unscaled = -unscaled;
    }
    // digits 共 15 位：value = digits × 10^(exp - 14)
    let mut scale = 14 - exp;
    if scale < 0 {
        let p =
            pow10((-scale) as u8).ok_or_else(|| EngineError::business(112, "numeric overflow"))?;
        unscaled = unscaled
            .checked_mul(p)
            .ok_or_else(|| EngineError::business(112, "numeric overflow"))?;
        scale = 0;
    }
    while scale > 0 && unscaled % 10 == 0 {
        unscaled /= 10;
        scale -= 1;
    }
    if unscaled == 0 {
        scale = 0;
    } else if scale > 38 {
        // i128 定标上界：宁可显式报错，不得静默截断（AGENTS §5）
        return Err(EngineError::business(112, "numeric overflow"));
    }
    Ok(SpinValue::Decimal(unscaled, 38, scale as u8))
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
        // GaussDB A 模式（Oracle 语义）实测：MOD(n, 0) = n（返回被除数，不报
        // 除零，0 % 0 = 0 自然成立）；int8 % -1 = 0（内核特判防 i64::MIN 溢出）；
        // 符号随被除数（-7 % 3 = -1，Rust rem 同口径）。BIZ-124 只保留给 `/ 0`
        BinOp::Mod if y == 0 => Ok(x),
        BinOp::Mod if y == -1 => Ok(0),
        BinOp::Mod => x.checked_rem(y).ok_or_else(overflow),
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
