//! 解析级变换与规则重命名单测（自 `rewrite.rs` 拆分，AGENTS §3；断言一字不改）。

use crate::sql::rewrite::{
    apply_ast_transforms, oracle_format_to_strftime, rewrite_tokens_with_rules,
};

#[test]
fn should_requote_embedded_format_mask() {
    // 回归守护：掩码内含单引号字面量时，unquote 后必须 requote 再嵌入，
    // 否则产出畸形 SQL（''y' 曾还原为 'y' 直接进入 strftime 字面量）
    // 掩码 x'y 经大写化后字面量 ' 必须以 '' 转义嵌入（requote），
    // 否则 'X'Y' 会让字符串提前闭合（畸形 SQL）
    assert_eq!(
        apply_ast_transforms("to_char(d, 'x''y')").unwrap(),
        "strftime(d, 'X''Y')"
    );
}

#[test]
fn should_not_panic_on_unterminated_string() {
    // 未闭合字符串吞掉右括号：不得越界 panic，原样返回交由下游报错
    let out = apply_ast_transforms("to_char(d, 'x''y'yyyymmdd')").unwrap();
    assert_eq!(out, "to_char(d, 'x''y'yyyymmdd')");
}

#[test]
fn should_transform_decode_to_case_with_default() {
    let r = apply_ast_transforms("SELECT decode(t.bs, '1', 'B', '-1', 'S', '0') FROM t").unwrap();
    assert!(
        r.contains("CASE t.bs WHEN '1' THEN 'B' WHEN '-1' THEN 'S' ELSE '0' END"),
        "{r}"
    );
}

#[test]
fn should_transform_nested_decode_innermost_first() {
    let r =
        apply_ast_transforms("SELECT decode(m, '001', decode(v, '0', 0, f), f) FROM t").unwrap();
    assert_eq!(r.matches("CASE").count(), 2, "{r}");
    assert!(!r.to_uppercase().contains("DECODE("), "{r}");
}

#[test]
fn should_not_touch_function_like_text_inside_string_literals() {
    // 字符串里的 to_char(...) 不是函数调用，必须原样保留
    let sql = "SELECT 'to_char(x, ''YYYY'')' AS s FROM t";
    let r = apply_ast_transforms(sql).unwrap();
    assert_eq!(r, sql, "{r}");
}

#[test]
fn should_transform_every_occurrence_in_one_statement() {
    let r = apply_ast_transforms("SELECT to_number(a), to_number(b) FROM t").unwrap();
    assert_eq!(r.matches("CAST(").count(), 2, "{r}");
    assert!(!r.to_ascii_uppercase().contains("TO_NUMBER("), "{r}");
}

#[test]
fn should_keep_commas_inside_literal_arguments() {
    // 字面量里的逗号不能当作参数分隔符
    let r = apply_ast_transforms("SELECT decode(k, ',', 'x', 'y') FROM t").unwrap();
    assert!(r.contains("WHEN ',' THEN 'x'"), "{r}");
    assert!(r.contains("ELSE 'y'"), "{r}");
}

#[test]
fn should_transform_to_number_to_cast() {
    let r = apply_ast_transforms("SELECT to_number(t.bs) * 2 FROM t").unwrap();
    assert!(r.contains("CAST(t.bs AS DECIMAL(38,6))"), "{r}");
}

#[test]
fn should_transform_to_char_with_oracle_format() {
    let r = apply_ast_transforms("SELECT to_char(now(), 'YYYYMMDD') FROM t").unwrap();
    assert!(r.contains("strftime(now(), '%Y%m%d')"), "{r}");
}

#[test]
fn should_transform_intertonum_with_wrapped_paren_expr() {
    // Bin 渲染带外层括号：intertonum((strptime(..) - strptime(..))) 也要可拆
    let r = apply_ast_transforms(
        "SELECT intervaltonum((strptime('20240110', '%Y%m%d') - strptime('20240101', '%Y%m%d'))) FROM sys_dummy",
    )
    .unwrap();
    assert!(
        r.contains(
            "date_diff('day', strptime('20240101', '%Y%m%d'), strptime('20240110', '%Y%m%d'))"
        ),
        "{r}"
    );
}

#[test]
fn should_transform_to_date_to_strptime() {
    let r = apply_ast_transforms("SELECT to_date(d, 'YYYYMMDD') FROM t").unwrap();
    assert_eq!(r, "SELECT strptime(d, '%Y%m%d') FROM t");
}

#[test]
fn should_transform_intertonum_to_date_diff() {
    // 语料 FNC_GET_PURCHASE_JS_DAYS：A - B → date_diff('day', B, A)
    let r = apply_ast_transforms(
        "SELECT intervaltonum(to_date('20240110', 'YYYYMMDD') - to_date('20240101', 'YYYYMMDD')) FROM sys_dummy",
    )
    .unwrap();
    // 内层 to_date 先行变换，intertonum 收到的操作数已是 strptime 形态
    assert_eq!(
        r,
        "SELECT date_diff('day', strptime('20240101', '%Y%m%d'), strptime('20240110', '%Y%m%d')) FROM sys_dummy",
        "{r}"
    );
}

#[test]
fn should_transform_to_char_of_to_date_with_interval() {
    let r =
        apply_ast_transforms("SELECT to_char(to_date(d, 'YYYYMMDD') + v_days, 'YYYYMMDD') FROM t")
            .unwrap();
    assert!(
        r.contains("strftime(strptime(d, '%Y%m%d') + v_days, '%Y%m%d')"),
        "{r}"
    );
}

#[test]
fn should_rename_simple_functions_via_token_pass() {
    let u = rewrite_tokens_with_rules(
        "SELECT nvl(a, sysdate), substr(b, 1, 2) FROM t WHERE x = p_i_date",
        &["p_i_date".to_string()].into_iter().collect(),
        &[
            ("NVL", "COALESCE"),
            ("SYSDATE", "CURRENT_TIMESTAMP"),
            ("SUBSTR", "SUBSTRING"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    )
    .unwrap();
    assert!(
        u.text.contains("COALESCE(a, CURRENT_TIMESTAMP)"),
        "{}",
        u.text
    );
    assert!(u.text.contains("SUBSTRING(b, 1, 2)"), "{}", u.text);
    assert!(u.text.contains("$v_p_i_date"), "{}", u.text);
}

#[test]
fn should_convert_oracle_format_tokens() {
    assert_eq!(oracle_format_to_strftime("YYYYMMDD"), "%Y%m%d");
    assert_eq!(
        oracle_format_to_strftime("YYYYMMDDHH24MISS"),
        "%Y%m%d%H%M%S"
    );
}
