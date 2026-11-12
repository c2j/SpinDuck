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
            // 十进制按普通小数展示（CLI OUT 渲染与日志可读性；不用科学计数法）
            SpinValue::Decimal(..) => self.as_str_loose(),
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
                // 负数必须整体带符号：`-0.005` 而非 `0.0-5`
                let neg = *v < 0;
                let abs = v.unsigned_abs();
                let div = 10u128.pow(*s as u32);
                let (ip, frac) = (abs / div, abs % div);
                let sign = if neg { "-" } else { "" };
                if *s == 0 {
                    format!("{sign}{ip}")
                } else {
                    format!("{sign}{ip}.{frac:0>w$}", w = *s as usize)
                }
            }
            SpinValue::Blob(_) => "<blob>".into(),
            SpinValue::Record(_) => "<record>".into(),
        }
    }
}

/// 写入已声明标量槽（第一阶段只处理 `VARCHAR`）。
///
/// `ty == None`、以及 `NUMBER` / `DATE` / `BLOB` 等非 `VARCHAR` 目标：原样返回。
/// 目标为 `VARCHAR` 时：`NULL` 与 `Str` 不动；`assignment_loose` 开启时
/// `Int` / `Decimal` / `Bool` 收成 `Str`；关闭时这些变体返回 `BIZ-0111`。
/// `Record` 与 `Blob` 不在本阶段转换。
pub fn coerce_for_slot(
    v: &SpinValue,
    ty: Option<&SpinType>,
    assignment_loose: bool,
) -> Result<SpinValue, EngineError> {
    if !matches!(ty, Some(SpinType::Varchar)) {
        return Ok(v.clone());
    }
    match v {
        SpinValue::Null | SpinValue::Str(_) => Ok(v.clone()),
        SpinValue::Int(_) | SpinValue::Decimal(..) | SpinValue::Bool(_) if assignment_loose => {
            Ok(SpinValue::Str(v.as_str_loose()))
        }
        // 码 111 与下方 coerce() 兜底共用。本路径的消息固定为赋值句，
        // 探针 should_reject_numeric_varchar_argument_when_assignment_loose_is_off 锁住它。
        SpinValue::Int(_) | SpinValue::Decimal(..) | SpinValue::Bool(_) => Err(
            EngineError::business(111, format!("cannot assign {} to VARCHAR", v.display())),
        ),
        SpinValue::Blob(_) | SpinValue::Record(_) => Ok(v.clone()),
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
    fn should_display_scalars_in_plain_form() {
        assert_eq!(SpinValue::Null.display(), "NULL");
        assert_eq!(SpinValue::Bool(true).display(), "true");
        assert_eq!(SpinValue::Int(-7).display(), "-7");
        assert_eq!(SpinValue::Str("F0001".into()).display(), "F0001");
        assert_eq!(SpinValue::Blob(vec![1, 2, 3]).display(), "<blob 3B>");
        // 十进制：普通小数形态，非科学计数法
        assert_eq!(SpinValue::Decimal(260, 38, 2).display(), "2.60");
        assert_eq!(SpinValue::Decimal(-5, 38, 3).display(), "-0.005");
    }

    #[test]
    fn should_assess_type_fit_for_each_value_kind() {
        use SpinType as T;
        let dec = T::Decimal {
            precision: 15,
            scale: 2,
        };
        assert!(SpinValue::Null.fits(&T::Int));
        assert!(SpinValue::Bool(true).fits(&T::Bool));
        assert!(SpinValue::Int(1).fits(&T::BigInt));
        assert!(SpinValue::Int(1).fits(&dec));
        assert!(SpinValue::Decimal(100, 38, 2).fits(&dec));
        assert!(SpinValue::Str("x".into()).fits(&T::Varchar));
        assert!(SpinValue::Blob(vec![0]).fits(&T::Blob));
        // 不匹配的组合必须为 false（否则隐式转换被绕过）
        assert!(!SpinValue::Int(1).fits(&T::Varchar));
        assert!(!SpinValue::Str("x".into()).fits(&T::Int));
        assert!(!SpinValue::Bool(true).fits(&T::Int));
        assert!(!SpinValue::Blob(vec![0]).fits(&T::Varchar));
        assert!(!SpinValue::Record(vec![]).fits(&T::Varchar));
    }

    #[test]
    fn should_report_invalid_numeric_literal_with_business_code() {
        let e = coerce(
            &SpinValue::Str("abc".into()),
            &SpinType::Decimal {
                precision: 15,
                scale: 2,
            },
        )
        .unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0112");
        assert!(e.message.contains("abc"));
    }

    #[test]
    fn should_coerce_string_to_int_and_bool_to_varchar() {
        assert_eq!(
            coerce(&SpinValue::Str("42".into()), &SpinType::Int).unwrap(),
            SpinValue::Decimal(42, 38, 0)
        );
        assert_eq!(
            coerce(&SpinValue::Bool(false), &SpinType::Varchar).unwrap(),
            SpinValue::Str("false".into())
        );
    }

    #[test]
    fn should_stringify_int_into_varchar_when_assignment_is_loose() {
        let v = coerce_for_slot(&SpinValue::Int(20260106), Some(&SpinType::Varchar), true).unwrap();
        assert_eq!(v, SpinValue::Str("20260106".into()));
    }

    #[test]
    fn should_keep_int_when_declared_slot_is_number() {
        let v = coerce_for_slot(
            &SpinValue::Int(42),
            Some(&SpinType::Decimal {
                precision: 38,
                scale: 6,
            }),
            true,
        )
        .unwrap();
        assert_eq!(v, SpinValue::Int(42));
    }

    #[test]
    fn should_reject_int_into_varchar_when_assignment_loose_is_off() {
        let e = coerce_for_slot(&SpinValue::Int(20260106), Some(&SpinType::Varchar), false)
            .unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0111");
        assert!(
            e.message.contains("cannot assign") && e.message.contains("VARCHAR"),
            "实际: {}",
            e.message
        );
    }

    #[test]
    fn should_stringify_decimal_and_bool_into_varchar_when_assignment_is_loose() {
        let d = coerce_for_slot(
            &SpinValue::Decimal(12, 38, 1),
            Some(&SpinType::Varchar),
            true,
        )
        .unwrap();
        assert_eq!(d, SpinValue::Str("1.2".into()));
        let b = coerce_for_slot(&SpinValue::Bool(true), Some(&SpinType::Varchar), true).unwrap();
        assert_eq!(b, SpinValue::Str("true".into()));
    }

    #[test]
    fn should_keep_str_null_and_untyped_slots_unchanged() {
        let s = SpinValue::Str("20260106".into());
        assert_eq!(
            coerce_for_slot(&s, Some(&SpinType::Varchar), false).unwrap(),
            s
        );
        assert_eq!(
            coerce_for_slot(&SpinValue::Null, Some(&SpinType::Varchar), false).unwrap(),
            SpinValue::Null
        );
        assert_eq!(
            coerce_for_slot(&SpinValue::Int(7), None, false).unwrap(),
            SpinValue::Int(7)
        );
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
