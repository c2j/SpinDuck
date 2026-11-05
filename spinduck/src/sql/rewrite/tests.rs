//! token 级改写单测（自 `rewrite.rs` 拆分，AGENTS §3；断言一字不改）。

use crate::ir::VarName;
use crate::sql::rewrite::{apply_ast_transforms, rewrite_tokens, SqlBindSrc};
use std::collections::BTreeSet;

fn vars(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn should_substitute_ident_tokens_with_named_placeholders() {
    let u = rewrite_tokens(
        "SELECT bcrq, scdm FROM mid_yjqs_detail WHERE bcrq = p_i_date AND scdm = '001'",
        &vars(&["p_i_date"]),
    )
    .unwrap();
    assert_eq!(
        u.text,
        "SELECT bcrq, scdm FROM mid_yjqs_detail WHERE bcrq = $v_p_i_date AND scdm = '001'"
    );
    assert_eq!(u.binds.len(), 1);
    assert_eq!(u.binds[0].1, SqlBindSrc::Var(VarName("p_i_date".into())));
}

#[test]
fn should_dedupe_binds_for_repeated_variables() {
    let u = rewrite_tokens(
        "SELECT a FROM t WHERE bcrq = p_i_date AND end_date = p_i_date",
        &vars(&["p_i_date"]),
    )
    .unwrap();
    assert_eq!(u.text.matches("$v_p_i_date").count(), 2);
    assert_eq!(u.binds.len(), 1); // 同名消重，绑定一次
}

#[test]
fn should_keep_string_literals_and_quoted_idents_unsubstituted() {
    let u = rewrite_tokens(
        "SELECT \"p_i_date\" FROM t WHERE note = 'p_i_date' AND x = p_i_date",
        &vars(&["p_i_date"]),
    )
    .unwrap();
    assert!(u.text.contains("'p_i_date'")); // 字符串字面量不动
    assert!(u.text.contains("\"p_i_date\"")); // 引号标识符不动
    assert_eq!(u.binds.len(), 1); // 只有裸标识符被替换
}

#[test]
fn should_cast_single_arg_to_char_to_varchar() {
    // 回归（语料 BIZ-0132）：1 参 TO_CHAR 曾直接报错，语料大量使用
    let out = apply_ast_transforms("SELECT TO_CHAR(123.450) FROM dual").unwrap();
    assert_eq!(out, "SELECT CAST(123.450 AS VARCHAR) FROM dual");
}

#[test]
fn should_rewrite_two_arg_to_char_with_oracle_format() {
    let out = apply_ast_transforms("SELECT TO_CHAR(d, 'YYYYMMDD') FROM dual").unwrap();
    assert!(out.contains("strftime(d, '%Y%m%d')"), "{out}");
}

#[test]
fn should_not_substitute_qualified_name_parts_sharing_a_var_name() {
    // 回归（语料 FNC_GET_STOCK_KIND）：局部变量与列同名时，
    // 限定名 `t.stock_kind` 的列部分曾被替换为占位符 → 内核绑定错误
    let u = rewrite_tokens(
        "SELECT t.stock_kind FROM dic_stock_kind_sub t WHERE x = stock_kind",
        &vars(&["stock_kind"]),
    )
    .unwrap();
    assert_eq!(
        u.text,
        "SELECT t.stock_kind FROM dic_stock_kind_sub t WHERE x = $v_stock_kind"
    );
    assert_eq!(u.binds.len(), 1);
    assert_eq!(u.binds[0].1, SqlBindSrc::Var(VarName("stock_kind".into())));
}

#[test]
fn should_resolve_placeholder_names_case_insensitively() {
    let u = rewrite_tokens("SELECT * FROM t WHERE d = P_I_DATE", &vars(&["p_i_date"])).unwrap();
    assert_eq!(u.binds[0].1, SqlBindSrc::Var(VarName("p_i_date".into())));
}
