//! SpinValue：PL 值系统（DuckDB Value 的引擎侧表示 + RECORD + NULL）。

use crate::error::EngineError;
use crate::types::SpinType;

/// PL 值（本文件不依赖 DuckDB API，保证单元测试纯净；转换在 exec 层完成）。
#[derive(Debug, Clone, PartialEq)]
pub enum SpinValue {
    Null,
    Bool(bool),
    Int(i64),
    Decimal(i128, u8, u8), // unscaled 值 + precision + scale
    Str(String),
    Blob(Vec<u8>),
    /// RECORD / %ROWTYPE：字段名有序列表 + 值（字段序以 OPEN 时 schema 为准）
    Record(Vec<(String, SpinValue)>),
}

impl SpinValue {
    /// 声明类型（coerce 的目标侧）是否可容纳该值（宽松转换的静态判定）。
    pub fn fits(&self, t: &SpinType) -> bool {
        matches!(
            (self, t),
            (SpinValue::Null, _)
                | (SpinValue::Bool(_), SpinType::Bool)
                | (
                    SpinValue::Int(_),
                    SpinType::Int | SpinType::BigInt | SpinType::Decimal { .. }
                )
                | (
                    SpinValue::Decimal(..),
                    SpinType::Decimal { .. } | SpinType::Int | SpinType::BigInt
                )
                | (SpinValue::Str(_), SpinType::Varchar)
                | (SpinValue::Blob(_), SpinType::Blob)
        )
    }

    /// 字符串形态（日志/trace 用；RECORD 递归展示字段名）。
    pub fn display(&self) -> String {
        match self {
            SpinValue::Null => "NULL".into(),
            SpinValue::Bool(b) => b.to_string(),
            SpinValue::Int(i) => i.to_string(),
            SpinValue::Decimal(v, _, s) => {
                let exp = format!("e-{}", *s as usize);
                format!("{v:>0width$}{exp}", width = 1)
            }
            SpinValue::Str(s) => s.clone(),
            SpinValue::Blob(b) => format!("<blob {}B>", b.len()),
            SpinValue::Record(fields) => format!(
                "({})",
                fields
                    .iter()
                    .map(|(n, v)| format!("{n}={}", v.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// 宽松数值语义：字符串可转数字（隐式转换规则 `[implicit]`）。
    pub fn as_str_loose(&self) -> String {
        match self {
            SpinValue::Null => "".into(),
            SpinValue::Str(s) => s.clone(),
            SpinValue::Bool(b) => b.to_string(),
            SpinValue::Int(i) => i.to_string(),
            SpinValue::Decimal(v, _, s) => {
                let div = 10i128.pow(*s as u32);
                let frac = format!("{:0>w$}", v % div, w = *s as usize);
                format!("{}.{}", v / div, frac)
            }
            SpinValue::Blob(_) => "<blob>".into(),
            SpinValue::Record(_) => "<record>".into(),
        }
    }
}

/// 隐式转换（规则文件 `[implicit]` 驱动的统一入口）。
/// 返回 Err(EngineError) 表示不可转换（产生 BIZ 域诊断）。
pub fn coerce(v: &SpinValue, target: &SpinType) -> Result<SpinValue, EngineError> {
    if v.fits(target) {
        return Ok(v.clone());
    }
    match (v, target) {
        // 字符串 → 数值（宽松）
        (SpinValue::Str(s), SpinType::Int | SpinType::BigInt) => parse_numeric(s, 0),
        (SpinValue::Str(s), SpinType::Decimal { scale, .. }) => parse_numeric(s, *scale),
        // 数值 → 字符串（拼接/落 VARCHAR 列）
        (SpinValue::Int(_) | SpinValue::Decimal(..), SpinType::Varchar) => {
            Ok(SpinValue::Str(v.as_str_loose()))
        }
        (SpinValue::Bool(b), SpinType::Varchar) => Ok(SpinValue::Str(b.to_string())),
        // RECORD 字段整体作为标量传出是不合法的
        (SpinValue::Record(_), _) => Err(EngineError::business(
            110,
            "cannot coerce RECORD to scalar type",
        )),
        _ => Err(EngineError::business(
            111,
            format!("cannot coerce {:?} to {:?}", v, target),
        )),
    }
}

fn parse_numeric(s: &str, scale: u8) -> Result<SpinValue, EngineError> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(SpinValue::Null);
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(SpinValue::Decimal(i as i128, 38, scale));
    }
    if let Ok(f) = t.parse::<f64>() {
        let div = 10f64.powi(scale as i32);
        return Ok(SpinValue::Decimal((f * div).round() as i128, 38, scale));
    }
    Err(EngineError::business(
        112,
        format!("invalid numeric literal: {s}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_null_for_empty_string_to_numeric() {
        let r = coerce(
            &SpinValue::Str("".into()),
            &SpinType::Decimal {
                precision: 15,
                scale: 2,
            },
        )
        .unwrap();
        assert_eq!(r, SpinValue::Null);
    }

    #[test]
    fn should_cast_numeric_string_to_decimal_loosely() {
        let r = coerce(
            &SpinValue::Str("123.45".into()),
            &SpinType::Decimal {
                precision: 15,
                scale: 2,
            },
        )
        .unwrap();
        assert_eq!(r, SpinValue::Decimal(12345, 38, 2));
    }

    #[test]
    fn should_concat_numeric_to_varchar_natively() {
        // V6 实测：'x'||DECIMAL 原生可用；引擎侧 coerce 到 VARCHAR 也必须成立
        let r = coerce(&SpinValue::Int(123), &SpinType::Varchar).unwrap();
        assert_eq!(r, SpinValue::Str("123".into()));
    }

    #[test]
    fn should_reject_record_to_scalar_coercion() {
        let rec = SpinValue::Record(vec![("a".into(), SpinValue::Int(1))]);
        assert!(coerce(&rec, &SpinType::Varchar).is_err());
    }

    #[test]
    fn should_display_record_with_field_names() {
        let rec = SpinValue::Record(vec![
            ("fund_code".into(), SpinValue::Str("F0001".into())),
            ("cjsl".into(), SpinValue::Int(100)),
        ]);
        assert_eq!(rec.display(), "(fund_code=F0001, cjsl=100)");
    }
}
