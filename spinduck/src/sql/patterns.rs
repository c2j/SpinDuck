//! `[patterns]` 规则的 AST/文本级改写：静态语句编译与动态 SQL 准备的**唯一实现点**。
//!
//! 两条路径曾各写一份 rownum/UPDATE 去别名逻辑（第三轮评审 Blocker：口径可漂移）。
//! 现统一由 `apply_text_patterns` 入口提供，改写产物差异只可能来自文本来源
//! （静态 DML 用 PL 解析器文本、动态用 SqlFormatter 文本），不再来自规则实现。

use crate::error::EngineError;
use crate::rules::RuleRegistry;
use ogsql_parser::ast::{
    Expr, InsertSource, JoinType, Literal, ObjectName, SelectStatement, SetOperation, Statement,
    TableRef,
};
use ogsql_parser::token::Token;

/// `[patterns]` 各改写共同的 AST 入口：仅在**语法错误**时保持原文，由后续 token
/// 改写与内核给出明确错误，不静默吞掉。`ParserError::Warning` 是提示级条目
/// （如 `(+)` 弃用告警），语句本身解析成功，不能当作失败。
fn reparse(text: &str) -> Option<Vec<ogsql_parser::ast::StatementInfo>> {
    let (infos, errors) = ogsql_parser::parser::Parser::parse_sql(text);
    let fatal = errors
        .iter()
        .any(|e| !matches!(e, ogsql_parser::parser::ParserError::Warning { .. }));
    if !fatal && !infos.is_empty() {
        Some(infos)
    } else {
        None
    }
}

/// 文本级 `[patterns]` 应用：`rownum = 1` → `LIMIT 1`、`UPDATE t SET t.c` 去别名。
///
/// rownum 改写需要 AST（谓词定位），仅在文本含 `rownum` 时回解析一次；
/// 回解析失败保持原文——由后续 token 改写与内核给出明确错误，不静默吞掉。
pub fn apply_text_patterns(sql_text: &str, rules: &RuleRegistry) -> Result<String, EngineError> {
    let mut text = sql_text.to_string();
    if rules.patterns_rownum_first_row && text.to_ascii_uppercase().contains("ROWNUM") {
        if let Some(infos) = reparse(&text) {
            let fmt = ogsql_parser::formatter::SqlFormatter::new();
            let parts: Vec<String> = infos
                .iter()
                .map(|si| {
                    let mut st = si.statement.clone();
                    apply_rownum_first_row(&mut st);
                    fmt.format_statement(&st)
                })
                .collect();
            text = parts.join(";\n");
        }
    }
    if rules.patterns_oracle_outer_join && text.contains("(+)") {
        text = rewrite_outer_join(&text);
    }
    if rules.patterns_update_strip_alias {
        text = super::strip_update_alias(&text);
    }
    if rules.patterns_sequence_nextval && text.to_ascii_uppercase().contains("NEXTVAL") {
        text = rewrite_sequence_nextval(&text);
    }
    // A 模式 `||` 把 NULL 侧当空串（基线实测 'A' || NULL = 'A'），DuckDB 传播 NULL
    // → 拼接链改写为内核 concat(...)；口径见 validation V21
    if rules.patterns_concat_null_as_empty && text.contains("||") {
        text = super::rewrite_concat(&text);
    }
    Ok(text)
}

/// `(+)` 外连接标记 → ANSI `LEFT JOIN … ON`。
///
/// 命中数口径：清单 `oracle_outer_join_marker`（`config/corpus_forms.toml:54` = 8）数的是
/// **出现次数**（正则 `\(\s*\+\s*\)`），且全部落在同一文件代号 F140——**不要**把"8 处"读成
/// "8 处都已成功改写"：适配形态在命中数里占多少没有同口径统计。交付件级命中面（数量级
/// 更大）与其后果登记在 `docs/validation.md` V27.2，守护用例
/// `tests/guard/guard_patterns_outer_join_residual.rs`。
///
/// 只在确实改写时重排文本；未命中的语句原样返回，避免无谓地把语料文本重排一遍。
/// 标记节点在 formatter 里还原成 `x(+)`（ogsql 无表达式就地改写入口，AGENTS §1
/// 只用公共 API），故改写产物再按 token 剥一次标记。
fn rewrite_outer_join(text: &str) -> String {
    let Some(infos) = reparse(text) else {
        return text.to_string();
    };
    let fmt = ogsql_parser::formatter::SqlFormatter::new();
    let mut parts: Vec<String> = Vec::with_capacity(infos.len());
    let mut changed = false;
    for si in &infos {
        let mut st = si.statement.clone();
        changed |= apply_outer_join_markers(&mut st);
        parts.push(fmt.format_statement(&st));
    }
    if changed {
        strip_outer_markers(&parts.join(";\n"))
    } else {
        text.to_string()
    }
}

/// 文本中 `(+)` 标记剥离。`(+)` 只可能是 `LParen Plus RParen` 三连
/// （`(+1)` 之类中间还有操作数，不匹配）；字符串字面量是独立 token，天然不受影响。
fn strip_outer_markers(text: &str) -> String {
    let Ok(tokens) = ogsql_parser::token::tokenizer::Tokenizer::new(text).tokenize() else {
        return text.to_string();
    };
    let is = |k: usize, t: Token| tokens[k].token == t;
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    let mut k = 0usize;
    while k + 2 < tokens.len() {
        if is(k, Token::LParen) && is(k + 1, Token::Plus) && is(k + 2, Token::RParen) {
            spans.push((tokens[k].span.start, tokens[k + 2].span.end, String::new()));
            k += 3;
        } else {
            k += 1;
        }
    }
    splice(text, &spans)
}

/// 语句级 `(+)` 标记改写；返回是否实际改写。
pub fn apply_outer_join_markers(stmt: &mut Statement) -> bool {
    use Statement as S;
    match stmt {
        S::Select(sel) => apply_on_body(&mut sel.node),
        S::Insert(ins) => match &mut ins.source {
            // 语料形态集中在 INSERT … SELECT（与 rownum 改写同一片区域）
            InsertSource::Select(q) => apply_on_body(q),
            _ => false,
        },
        // UPDATE/DELETE 的 `(+)`：语料未出现。多表 UPDATE 的 SET 目标表与 FROM
        // 源表分列两个清单，连接顺序语义不确定，保守不动（内核给出明确错误）
        _ => false,
    }
}

/// SELECT 体：本级改写 + UNION 族右支递归。
fn apply_on_body(sel: &mut SelectStatement) -> bool {
    let mut done = apply_on_from_where(&mut sel.from, &mut sel.where_clause);
    if let Some(op) = &mut sel.set_operation {
        let rhs = match op {
            SetOperation::Union { right, .. }
            | SetOperation::Intersect { right, .. }
            | SetOperation::Except { right, .. } => right,
        };
        done |= apply_on_body(rhs);
    }
    done
}

/// `from` 逗号表清单 + `where` 的 `(+)` 谓词 → 末表 `LEFT JOIN … ON`。
///
/// 保守边界：至少两张表、全部标记落在同一张表且该表位于清单末位、谓词按顶层 AND
/// 链组织。标记侧必须是末表——`LEFT JOIN` 只能接在连接链末端，插在中间会改连接顺序语义。
/// 不适配时**原样保留标记**，由内核在解析阶段拒绝（实测 `Parser Error: syntax error
/// at or near ")"`，守护见 `tests/guard/guard_patterns_outer_join_residual.rs`）。
/// 不声称"语料只出现适配形态"：形态占比没有同口径统计。
fn apply_on_from_where(from: &mut Vec<TableRef>, where_clause: &mut Option<Expr>) -> bool {
    let Some(w) = where_clause.take() else {
        return false;
    };
    let (marked, rest) = split_outer_markers(&w);
    let bail = |where_clause: &mut Option<Expr>| {
        *where_clause = Some(w.clone());
        false
    };
    let (Some(side), Some(on)) = (marked_alias(&marked), and_chain(marked)) else {
        return bail(where_clause);
    };
    if from.len() < 2 || ref_alias(&from[from.len() - 1]) != Some(side) {
        return bail(where_clause);
    }
    // 前置表折成 CROSS JOIN 作为 LEFT 侧：`(A, B) LEFT JOIN C ON A.k = C.k AND B.j = C.j`
    // 若保留逗号，ON 里跨逗号引用会被内核判为非法表引用
    let mut refs = std::mem::take(from);
    let right = match refs.pop() {
        Some(r) => r,
        None => {
            *from = refs;
            return bail(where_clause);
        }
    };
    let mut left = match refs.pop() {
        Some(l) => l,
        None => {
            refs.push(right);
            *from = refs;
            return bail(where_clause);
        }
    };
    while let Some(t) = refs.pop() {
        left = cross_join(t, left);
    }
    *from = vec![TableRef::Join {
        left: Box::new(left),
        right: Box::new(right),
        join_type: JoinType::Left,
        condition: Some(on),
        natural: false,
        using_columns: Vec::new(),
    }];
    *where_clause = and_chain(rest);
    true
}

/// 顶层 AND 链拆成谓词，按是否含 `(+)` 标记分组：(标记谓词, 其余谓词)。
fn split_outer_markers(w: &Expr) -> (Vec<Expr>, Vec<Expr>) {
    let (mut marked, mut rest) = (Vec::new(), Vec::new());
    for c in split_and_chain(w) {
        if has_outer_marker(&c) {
            marked.push(c);
        } else {
            rest.push(c);
        }
    }
    (marked, rest)
}

fn split_and_chain(e: &Expr) -> Vec<Expr> {
    if let Expr::BinaryOp { op, left, right } = e {
        if op.eq_ignore_ascii_case("AND") {
            let mut out = split_and_chain(left);
            out.extend(split_and_chain(right));
            return out;
        }
    }
    vec![e.clone()]
}

fn has_outer_marker(e: &Expr) -> bool {
    let mut hit = false;
    ogsql_parser::linter::walk_expr(e, &mut |x| {
        if matches!(x, Expr::ColumnRefOuterJoin(_)) {
            hit = true;
        }
        true
    });
    hit
}

/// 全部标记谓词的宿主别名（`ObjectName` 首段）。多于一张表 → None：
/// 一个 `LEFT JOIN` 表达不了两侧都补空的形态（那是 FULL JOIN 的 Oracle 写法）。
fn marked_alias(marked: &[Expr]) -> Option<String> {
    let mut side: Option<String> = None;
    for e in marked {
        let names = outer_join_names(e);
        if names.is_empty() {
            return None;
        }
        for name in names {
            let head = name.first()?.value.clone();
            match &side {
                Some(s) if !s.eq_ignore_ascii_case(&head) => return None,
                Some(_) => {}
                None => side = Some(head),
            }
        }
    }
    side
}

fn outer_join_names(e: &Expr) -> Vec<ObjectName> {
    let mut out = Vec::new();
    ogsql_parser::linter::walk_expr(e, &mut |x| {
        if let Expr::ColumnRefOuterJoin(name) = x {
            out.push(name.clone());
        }
        true
    });
    out
}

/// 表引用对外可见的名字：显式别名优先，否则取表名末段（与 `(+)` 前缀同源）。
fn ref_alias(t: &TableRef) -> Option<String> {
    match t {
        TableRef::Table { name, alias, .. } => Some(
            alias
                .as_ref()
                .or_else(|| name.last())
                .map(|i| i.value.clone())?,
        ),
        _ => None,
    }
}

fn cross_join(left: TableRef, right: TableRef) -> TableRef {
    TableRef::Join {
        left: Box::new(left),
        right: Box::new(right),
        join_type: JoinType::Cross,
        condition: None,
        natural: false,
        using_columns: Vec::new(),
    }
}

/// 谓词折回 AND 链；空清单 → None（对应 WHERE 整体搬进 ON 的情形）。
fn and_chain(es: Vec<Expr>) -> Option<Expr> {
    es.into_iter().reduce(|a, b| Expr::BinaryOp {
        op: "AND".to_string(),
        left: Box::new(a),
        right: Box::new(b),
    })
}

/// `seq.NEXTVAL` → `nextval('seq')`（文本级，覆盖 SELECT/INSERT/UPDATE/WHERE 全部位置）。
///
/// 只认「标识符 . NEXTVAL」且前面不是点号（`a.b.nextval` 属列名，不动）；
/// 字符串字面量是独立 token，天然不会被改写。
fn rewrite_sequence_nextval(text: &str) -> String {
    let Ok(tokens) = ogsql_parser::token::tokenizer::Tokenizer::new(text).tokenize() else {
        return text.to_string();
    };
    let identish = |k: usize| {
        matches!(
            tokens.get(k),
            Some(t) if matches!(t.token,
                ogsql_parser::token::Token::Ident(_)
                    | ogsql_parser::token::Token::Keyword(_)
                    | ogsql_parser::token::Token::QuotedIdent(_))
        )
    };
    let word = |k: usize| &text[tokens[k].span.start..tokens[k].span.end];
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 1usize;
    while i + 1 < tokens.len() {
        let dotted = matches!(tokens[i - 1].token, ogsql_parser::token::Token::Dot);
        let attr = word(i + 1).eq_ignore_ascii_case("nextval");
        let not_more = !matches!(tokens.get(i + 2), Some(t) if matches!(t.token, ogsql_parser::token::Token::Dot));
        if !dotted
            && identish(i - 1)
            && matches!(tokens[i].token, ogsql_parser::token::Token::Dot)
            && identish(i + 1)
            && attr
            && not_more
        {
            spans.push((
                tokens[i - 1].span.start,
                tokens[i + 1].span.end,
                format!("nextval('{}')", word(i - 1).trim_matches('"')),
            ));
            i += 2;
        } else {
            i += 1;
        }
    }
    splice(text, &spans)
}

/// 按 span 批量替换（span 必须升序且不重叠——两处 token 级改写都由左到右扫描产出）。
fn splice(text: &str, spans: &[(usize, usize, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (s, e, rep) in spans {
        out.push_str(&text[cursor..*s]);
        out.push_str(rep);
        cursor = *e;
    }
    out.push_str(&text[cursor..]);
    out
}

/// 语句级 `rownum = 1` 首行改写；返回是否实际改写。
pub fn apply_rownum_first_row(stmt: &mut Statement) -> bool {
    use Statement as S;
    match stmt {
        S::Select(sel) => apply_on_select(&mut sel.node),
        S::Insert(ins) => match &mut ins.source {
            // 语料的 rownum 用法集中在 INSERT ... SELECT（42 处，validation V13）
            InsertSource::Select(q) => apply_on_select(q),
            // VALUES / DEFAULT VALUES / SET / RECORD 变量形态无 WHERE 子句，
            // 不存在 rownum 谓词位置
            _ => false,
        },
        // UPDATE/DELETE 的 rownum 谓词不改变结果集上限语义（DuckDB 无 rownum，
        // 改写为 LIMIT 需要子查询包裹，语料未出现该形态）；M8 若遇到再扩
        _ => false,
    }
}

/// SELECT 体：摘除 `rownum = 1` 谓词并补 `LIMIT 1`；已有 LIMIT 时保守还原条件。
pub fn apply_on_select(sel: &mut SelectStatement) -> bool {
    let Some(w) = sel.where_clause.take() else {
        return false;
    };
    let (rest, stripped) = strip_rownum_first(w.clone());
    if !stripped || sel.limit.is_some() {
        // 未命中，或已有 LIMIT（不可静默覆盖既有行数语义）：原样保留
        sel.where_clause = Some(w);
        return false;
    }
    sel.where_clause = rest;
    sel.limit = Some(Expr::Literal(Literal::Integer(1)));
    true
}

/// 递归剥除 AND 链中的 `rownum = 1` 谓词。返回 (剩余条件, 是否剥除)。
/// 仅处理 AND 链；OR/NOT 分支内的 rownum 保守保留（不改写）。
pub fn strip_rownum_first(e: Expr) -> (Option<Expr>, bool) {
    if is_rownum_first(&e) {
        return (None, true);
    }
    if let Expr::BinaryOp { op, left, right } = &e {
        if op.eq_ignore_ascii_case("AND") {
            let (l, ls) = strip_rownum_first((**left).clone());
            let (r, rs) = strip_rownum_first((**right).clone());
            let stripped = ls || rs;
            let rest = match (l, r) {
                (Some(l), Some(r)) => Some(Expr::BinaryOp {
                    op: op.clone(),
                    left: Box::new(l),
                    right: Box::new(r),
                }),
                (one, None) | (None, one) => one,
            };
            return (rest, stripped);
        }
    }
    (Some(e), false)
}

fn is_rownum_first(e: &Expr) -> bool {
    let Expr::BinaryOp { op, left, right } = e else {
        return false;
    };
    if !op.eq_ignore_ascii_case("=") {
        return false;
    }
    let is_one = |x: &Expr| matches!(x, Expr::Literal(Literal::Integer(1)));
    let is_rownum = |x: &Expr| {
        matches!(x, Expr::ColumnRef(id) | Expr::PlVariable(id)
        if id.len() == 1 && id[0].value.eq_ignore_ascii_case("rownum"))
    };
    (is_rownum(left) && is_one(right)) || (is_one(left) && is_rownum(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(sql: &str) -> Statement {
        let (infos, errors) = ogsql_parser::parser::Parser::parse_sql(sql);
        assert!(errors.is_empty(), "解析失败: {errors:?}");
        infos[0].statement.clone()
    }

    #[test]
    fn should_add_limit1_when_stripping_rownum_from_select() {
        let mut st = parse_one("SELECT a FROM src WHERE rownum = 1");
        assert!(apply_rownum_first_row(&mut st));
        let out = ogsql_parser::formatter::SqlFormatter::new().format_statement(&st);
        assert!(
            !out.to_ascii_uppercase().contains("ROWNUM"),
            "rownum 必须摘除，实际: {out}"
        );
        assert!(
            out.to_ascii_uppercase().contains("LIMIT"),
            "必须补 LIMIT 1，实际: {out}"
        );
    }

    #[test]
    fn should_strip_rownum_from_insert_select() {
        // 语料主形态：INSERT INTO t SELECT ... WHERE rownum = 1（曾只在顶层 SELECT 生效）
        let mut st = parse_one("INSERT INTO tgt SELECT a FROM src WHERE rownum = 1");
        assert!(apply_rownum_first_row(&mut st), "INSERT...SELECT 必须下钻");
        let out = ogsql_parser::formatter::SqlFormatter::new().format_statement(&st);
        assert!(!out.to_ascii_uppercase().contains("ROWNUM"), "实际: {out}");
    }

    #[test]
    fn should_keep_existing_limit_and_rownum_untouched() {
        let mut st = parse_one("SELECT a FROM src WHERE rownum = 1 LIMIT 5");
        assert!(!apply_rownum_first_row(&mut st), "已有 LIMIT：不得覆盖");
        let out = ogsql_parser::formatter::SqlFormatter::new().format_statement(&st);
        assert!(
            out.to_ascii_uppercase().contains("ROWNUM"),
            "保守还原条件，实际: {out}"
        );
    }

    #[test]
    fn should_text_pattern_honour_rule_switches() {
        let mut off = RuleRegistry::default();
        off.overlay("[patterns]\nrownum_first_row = false\nupdate_strip_alias = false")
            .unwrap();
        let kept = apply_text_patterns("SELECT a FROM src WHERE rownum = 1", &off).unwrap();
        assert!(
            kept.to_ascii_uppercase().contains("ROWNUM"),
            "关闭开关不得改写"
        );
        let on = RuleRegistry::default();
        let rewritten = apply_text_patterns("SELECT a FROM src WHERE rownum = 1", &on).unwrap();
        assert!(
            !rewritten.to_ascii_uppercase().contains("ROWNUM"),
            "默认开启必须改写: {rewritten}"
        );
    }

    #[test]
    fn should_rewrite_sequence_nextval_to_kernel_function() {
        // 语料形态 `seq.NEXTVAL`（12 处：INSERT 列、SELECT … INTO）；DuckDB 只认
        // nextval('seq')，不改写必然报 "Referenced table \"seq\" not found"
        let on = RuleRegistry::default();
        let got = apply_text_patterns("INSERT INTO t (id, v) VALUES (seq_log.nextval, 'x')", &on)
            .unwrap();
        assert_eq!(
            got, "INSERT INTO t (id, v) VALUES (nextval('seq_log'), 'x')",
            "{got}"
        );
        let got = apply_text_patterns("SELECT SEQ_EXT.NEXTVAL INTO v FROM t", &on).unwrap();
        assert!(
            got.to_ascii_lowercase().contains("nextval('seq_ext')"),
            "{got}"
        );
    }

    #[test]
    fn should_not_rewrite_nextval_like_names_or_literals() {
        let on = RuleRegistry::default();
        // 字符串内的同形文本、非 nextval 的列名、裸 nextval（无序列前缀）都不得动
        let kept = apply_text_patterns(
            "SELECT a FROM t WHERE b = 'seq.NEXTVAL' AND c = d.nextval_x AND e = nextval",
            &on,
        )
        .unwrap();
        assert!(kept.contains("'seq.NEXTVAL'"), "{kept}");
        assert!(kept.contains("d.nextval_x"), "{kept}");
        assert!(kept.contains("e = nextval"), "{kept}");
    }

    #[test]
    fn should_leave_nextval_untouched_when_rule_is_off() {
        let mut off = RuleRegistry::default();
        off.overlay("[patterns]\nsequence_nextval = false").unwrap();
        let kept = apply_text_patterns("SELECT s.NEXTVAL FROM t", &off).unwrap();
        assert!(kept.to_ascii_uppercase().contains("S.NEXTVAL"), "{kept}");
    }

    #[test]
    fn should_convert_oracle_outer_join_marker_to_left_join() {
        // 语料主流形态（PKG_SPLIT_TRADE_STEP2.sql:168-175 等 8 处）：逗号 FROM +
        // 末表带标记；DuckDB 不认 `(+)`，解析阶段即 Parser Error
        let on = RuleRegistry::default();
        let got = apply_text_patterns(
            "SELECT t.gddm, pt.hslx FROM tsgh_temp t, par_tsdata_hslx pt \
             WHERE t.status = '1' AND t.gddm = pt.gddm(+) \
             AND t.bcrq BETWEEN pt.inure_begin_date(+) AND pt.inure_end_date(+)",
            &on,
        )
        .unwrap()
        .to_ascii_uppercase();
        assert!(!got.contains("(+)"), "标记必须全部剥除: {got}");
        assert!(got.contains("LEFT JOIN"), "必须改为 LEFT JOIN: {got}");
        assert!(got.contains(" ON "), "标记谓词必须进 ON: {got}");
        assert!(
            got.contains("CROSS JOIN")
                || got.find("LEFT JOIN").unwrap() < got.find("WHERE").unwrap(),
            "表清单必须落在 FROM 而非 WHERE 之后: {got}"
        );
        assert!(got.contains("STATUS = '1'"), "非标记谓词留在 WHERE: {got}");
    }

    #[test]
    fn should_fold_comma_tables_before_marker_into_cross_join() {
        // 三表逗号清单 + 标记全在末表：前置表折成 CROSS JOIN 作 LEFT 侧，
        // 否则 `ON` 里跨逗号引用前置表会被内核判为非法引用
        let on = RuleRegistry::default();
        let got = apply_text_patterns(
            "SELECT a.id FROM ta a, tb b, tc c \
             WHERE a.k = c.k(+) AND b.j = c.j(+)",
            &on,
        )
        .unwrap();
        let up = got.to_ascii_uppercase();
        assert!(!up.contains("(+)"), "{got}");
        assert!(up.contains("CROSS JOIN"), "前置表必须叉乘: {got}");
        assert!(up.contains("LEFT JOIN"), "{got}");
        assert!(
            up.contains(" LEFT JOIN TC AS C ON A.K = C.K AND B.J = C.J"),
            "标记谓词全部进 ON，逗号清单折成叉乘左侧: {got}"
        );
    }

    #[test]
    fn should_not_rewrite_outer_join_when_marker_side_is_not_last() {
        // 标记侧不在清单末位：LEFT JOIN 只能接在链尾，插在中间会改连接顺序语义；
        // 保守原样交给内核报错（语料未出现该形态）
        let on = RuleRegistry::default();
        let src = "SELECT a.id, b.id, c.id FROM ta a, tb b, tc c WHERE a.id = b.id(+)";
        let got = apply_text_patterns(src, &on).unwrap();
        assert!(!got.to_ascii_uppercase().contains("JOIN"), "{got}");
    }

    #[test]
    fn should_not_rewrite_outer_join_across_two_marker_sides() {
        // 两侧各有标记（一个 FULL 语义的 Oracle 写法）：一行 LEFT JOIN 表达不了，不改写
        let on = RuleRegistry::default();
        let src = "SELECT a.id FROM ta a, tb b WHERE a.id = b.id(+) AND b.x = a.y(+)";
        let got = apply_text_patterns(src, &on).unwrap();
        assert!(!got.to_ascii_uppercase().contains("JOIN"), "{got}");
    }

    #[test]
    fn should_leave_outer_join_untouched_when_rule_is_off() {
        let mut off = RuleRegistry::default();
        off.overlay("[patterns]\noracle_outer_join = false")
            .unwrap();
        let kept =
            apply_text_patterns("SELECT a.id FROM ta a, tb b WHERE a.id = b.id(+)", &off).unwrap();
        assert!(kept.contains("(+)"), "{kept}");
    }
}
