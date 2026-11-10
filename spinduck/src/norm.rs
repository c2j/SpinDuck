//! 逐值归一：黄金层与 `compare` 唯一的口径（design §6.2 A5）。
//!
//! 落地前这份逻辑在本仓有**两份逐字相同**的实现（`tests/common/mod.rs`、
//! `tests/corpus_forms/mod.rs`），只有测试目录能看见，CLI 链上的 `compare` 无从复用——
//! 于是"参照侧与本地各判一次"的口径迟早漂移。收口到 core 后两侧（以及 `compare`）
//! 判的是同一个函数，测试目录只改引用、不改任何断言。
//!
//! 口径本身一个字没动（下列用例即从两份实现的既有行为逐条锁定）：
//! `NULL` 显式化、`+` 前缀丢弃、空串即 `NULL`（A 模式 Oracle 语义）、
//! `.5` 补零成 `0.5`、小数去尾零且全零归 `0`。

use crate::value::SpinValue;

/// 归一一个单元格文本（`None` = 数据库 NULL）。
pub fn norm(s: Option<String>) -> String {
    let Some(t) = s else {
        return "NULL".to_string();
    };
    let t = t.trim().trim_start_matches('+');
    // A 模式（Oracle 语义）：空字符串即 NULL，两侧归一同口径
    if t.is_empty() {
        return "NULL".to_string();
    }
    let t = if let Some(rest) = t.strip_prefix('.') {
        format!("0.{rest}")
    } else if let Some(rest) = t.strip_prefix("-.") {
        format!("-0.{rest}")
    } else {
        t.to_string()
    };
    if t.contains('.') {
        let t2 = t.trim_end_matches('0').trim_end_matches('.');
        if t2.is_empty() || t2 == "-" {
            "0".to_string()
        } else {
            t2.to_string()
        }
    } else {
        t
    }
}

/// 引擎值 → 归一文本（`Null` 显式成 `"NULL"`，其余走显示串）。
pub fn norm_value(v: &SpinValue) -> String {
    match v {
        SpinValue::Null => "NULL".to_string(),
        other => norm(Some(other.as_str_loose())),
    }
}

/// 可缺省的引擎值（标量表达式结果）→ 归一文本。
pub fn norm_scalar(v: Option<SpinValue>) -> String {
    match v {
        None | Some(SpinValue::Null) => "NULL".to_string(),
        Some(other) => norm(Some(other.as_str_loose())),
    }
}

#[cfg(test)]
mod tests {
    use super::norm;
    use crate::value::SpinValue;

    #[test]
    fn should_render_none_as_null_literal() {
        assert_eq!(norm(None), "NULL");
    }

    #[test]
    fn should_treat_empty_and_blank_text_as_null() {
        // A 模式（Oracle 语义）：空字符串即 NULL，两侧归一同口径
        assert_eq!(norm(Some("".into())), "NULL");
        assert_eq!(norm(Some("   ".into())), "NULL");
    }

    #[test]
    fn should_trim_and_drop_leading_plus() {
        assert_eq!(norm(Some("  +42 ".into())), "42");
    }

    #[test]
    fn should_pad_leading_dot_with_zero() {
        assert_eq!(norm(Some(".5".into())), "0.5");
        assert_eq!(norm(Some("-.5".into())), "-0.5");
    }

    #[test]
    fn should_strip_trailing_zeros_and_dot() {
        assert_eq!(norm(Some("12.3400".into())), "12.34");
        assert_eq!(norm(Some("100.00".into())), "100");
        assert_eq!(norm(Some(".000".into())), "0");
        // 负号在去尾零后不消失：`-.0` → `-0.0` → `-0`（既有两份实现都是这个结果，逐字保留）
        assert_eq!(norm(Some("-.0".into())), "-0");
    }

    #[test]
    fn should_keep_integer_text_untouched() {
        // 无小数点时不去尾零：'100' 与 '100.00' 归一到同一个值，但 '100' 自己不能被削成 '1'
        assert_eq!(norm(Some("100".into())), "100");
        assert_eq!(norm(Some("-0".into())), "-0");
    }

    #[test]
    fn should_leave_non_numeric_text_as_trimmed_itself() {
        assert_eq!(norm(Some(" ABC ".into())), "ABC");
    }

    #[test]
    fn should_render_engine_values_through_the_same_norm() {
        // 值侧的两个包装只是"取显示串 + 归一"，不得再有第二套口径
        use crate::norm::{norm_scalar, norm_value};
        assert_eq!(norm_value(&SpinValue::Null), "NULL");
        assert_eq!(norm_value(&SpinValue::Int(7)), "7");
        assert_eq!(norm_value(&SpinValue::Decimal(4500, 38, 2)), "45");
        assert_eq!(norm_scalar(None), "NULL");
        assert_eq!(norm_scalar(Some(SpinValue::Str(" x ".into()))), "x");
    }
}
