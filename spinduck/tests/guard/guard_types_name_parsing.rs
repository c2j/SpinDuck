//! 回归守护（AGENTS §4.3）：GaussDB 类型名解析的畸形输入。
//!
//! `SpinType::from_gauss_name` 曾对 `NUMBER(`（无右括号）做 `base[i+1..len-1]`
//! 切片 → `start > end` panic；`NUMBER(15,x)` 的坏 scale 曾静默变 0。

use spinduck_core::types::{default_type_mapping, SpinType};

#[test]
fn should_return_none_for_unterminated_precision() {
    let m = default_type_mapping();
    // 曾 panic：&base[7..6]
    assert_eq!(SpinType::from_gauss_name("NUMBER(", &m), None);
    assert_eq!(SpinType::from_gauss_name("NUMBER(15", &m), None);
}

#[test]
fn should_return_none_for_bad_scale() {
    let m = default_type_mapping();
    // 曾静默变 scale=0；现显式 None（上层报未知类型）
    assert_eq!(SpinType::from_gauss_name("NUMBER(15,x)", &m), None);
}

#[test]
fn should_still_parse_valid_precision() {
    let m = default_type_mapping();
    assert_eq!(
        SpinType::from_gauss_name("NUMBER(15,2)", &m),
        Some(SpinType::Decimal {
            precision: 15,
            scale: 2
        })
    );
    assert_eq!(
        SpinType::from_gauss_name("VARCHAR2(4000)", &m),
        Some(SpinType::Varchar)
    );
}
