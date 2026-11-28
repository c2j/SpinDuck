//! token 级改写遍历：PL 变量 → 命名占位符，`[functions]` 重命名（AGENTS §3 规模拆分）。

use crate::error::EngineError;
use crate::ir::BindSrc;
use crate::ir::VarName;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::{Token, TokenWithSpan};

use std::collections::{BTreeMap, BTreeSet};

/// 改写产物：占位符化 SQL 文本 + 按名绑定表（同名消重，保序）。
#[derive(Debug, Clone, PartialEq)]
pub struct SqlUnit {
    pub text: String,
    pub binds: Vec<(String, SqlBindSrc)>,
    /// SQL 上下文例程调用下沉计划（`sqlfn::plan_sites` 填充；默认空）
    pub fn_sites: Vec<crate::sql::FnSite>,
}

/// token 级变量替换：裸标识符且命中 PL 变量集 → `$v_<小写名>`。
/// 引号标识符、字符串字面量、数字等 token 原样保留（字节切片重组）。
pub fn rewrite_tokens(sql_text: &str, var_names: &VarScope) -> Result<SqlUnit, EngineError> {
    let tokens = Tokenizer::new(sql_text)
        .tokenize()
        .map_err(|e| EngineError::invariant(40, format!("tokenizer: {e}")))?;
    let sql_names = collect_sql_names(sql_text, &tokens);
    let mut guard = SqlNameGuard::new(sql_names);
    let mut out = String::with_capacity(sql_text.len());
    let mut binds: Vec<(String, SqlBindSrc)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = 0usize;
    // 限定名保护：前一个 token 为 '.' 时当前 Ident 是限定名的后半段
    // （table.column / package.routine），不是 PL 变量，禁止替换
    let mut prev_was_dot = false;
    let mut i = 0usize;
    while i < tokens.len() {
        let tws = &tokens[i];
        let gap = sql_text
            .get(cursor..tws.span.start)
            .ok_or_else(|| EngineError::invariant(41, "token span out of order".to_string()))?;
        out.push_str(gap);
        let action = guard.protect(sql_text, &tokens, i, tws);
        let protected = matches!(action, GuardAction::Protect);
        match &tws.token {
            Token::Ident(value) => {
                let lower = value.to_ascii_lowercase();
                if protected {
                    // SQL 域标识符（表名/别名/别名限定前缀/INSERT 列/AS 别名）：
                    // 原样保留，不替换（V60；守护 guard_sql_name_resolution.rs）
                    out.push_str(&sql_text[tws.span.start..tws.span.end]);
                    prev_was_dot = false;
                    cursor = tws.span.end;
                    i += 1;
                    continue;
                }
                if !prev_was_dot {
                    if let Some(canon) = var_names.resolve_opt(&lower) {
                        let ph = format!("$v_{canon}");
                        out.push_str(&ph);
                        if seen.insert(ph.clone()) {
                            binds.push((ph, SqlBindSrc::Var(VarName(canon.to_string()))));
                        }
                        prev_was_dot = false;
                        cursor = tws.span.end;
                        i += 1;
                        continue;
                    }
                }
                out.push_str(&sql_text[tws.span.start..tws.span.end]);
            }
            Token::Dot => {
                out.push_str(&sql_text[tws.span.start..tws.span.end]);
                prev_was_dot = true;
                cursor = tws.span.end;
                i += 1;
                continue;
            }
            _ => out.push_str(&sql_text[tws.span.start..tws.span.end]),
        }
        prev_was_dot = false;
        cursor = tws.span.end;
        i += 1;
    }
    out.push_str(&sql_text[cursor..]);
    Ok(SqlUnit {
        text: out,
        binds,
        fn_sites: Vec::new(),
    })
}

/// PL 变量作用域：表层名（小写）→ 帧内规范名。
///
/// 嵌套 `DECLARE` 遮蔽外层同名变量、`FOR` 循环变量遮蔽外层绑定时，
/// 内层声明经 [`VarScope::declare`] 获得新规范名；块内引用按映射解析到
/// 内层槽位、离开块后恢复映射仍解析到外层——等价 PL/SQL 词法作用域，
/// 且对 GOTO/EXIT/异常等非本地跳转天然安全（帧保持扁平，无需作用域栈
/// 回卷）。预扫描收集的引用占位（[`VarScope::insert`]）不构成绑定，
/// 不会触发遮蔽改名。
#[derive(Debug, Default, Clone)]
pub struct VarScope {
    /// 表层名 → 规范名（未遮蔽时二者相同）。
    map: std::collections::BTreeMap<String, String>,
    /// 已被真实绑定（声明/参数/循环变量）的规范名集合。
    bound: std::collections::BTreeSet<String>,
    /// 遮蔽规范名序号（`__sc<N>_<surface>`）。
    next: usize,
}

/// 一次遮蔽声明的回卷信息：离开声明块时把表层名映射还原。
#[derive(Debug)]
pub struct VarRestore {
    surface: String,
    /// 声明前的映射值；`None` = 表层名此前未登记，回卷时移除。
    prev: Option<String>,
}

impl VarScope {
    pub fn new() -> Self {
        Self::default()
    }

    /// 预扫描/占位登记：只保证表层名可解析，不构成绑定。
    pub fn insert(&mut self, surface: String) {
        self.map.entry(surface.clone()).or_insert(surface);
    }

    /// 表层名是否已登记（含占位）。
    pub fn contains(&self, surface: &str) -> bool {
        self.map.contains_key(surface)
    }

    /// 表层名 → 当前规范名；未登记返回 `None`。
    pub fn resolve_opt(&self, surface: &str) -> Option<&str> {
        self.map.get(surface).map(String::as_str)
    }

    /// 真实绑定登记（DECLARE 变量、例程参数、FOR 循环变量）：
    /// 表层名已被外层绑定时生成遮蔽规范名，否则规范名即表层名。
    /// 返回声明的规范名与回卷信息（嵌套块结束时 [`VarScope::restore`]）。
    pub fn declare(&mut self, surface: &str) -> (String, VarRestore) {
        let prev = self.map.get(surface).cloned();
        let canon = match &prev {
            Some(c) if self.bound.contains(c) => {
                self.next += 1;
                format!("__sc{}_{}", self.next, surface)
            }
            _ => surface.to_string(),
        };
        self.map.insert(surface.to_string(), canon.clone());
        self.bound.insert(canon.clone());
        (
            canon,
            VarRestore {
                surface: surface.to_string(),
                prev,
            },
        )
    }

    /// 离开声明块：把表层名映射还原到声明前的状态。
    pub fn restore(&mut self, restore: &VarRestore) {
        match &restore.prev {
            Some(prev) => {
                self.map.insert(restore.surface.clone(), prev.clone());
            }
            None => {
                self.map.remove(&restore.surface);
            }
        }
    }
}

impl FromIterator<String> for VarScope {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        let mut scope = VarScope::new();
        for surface in iter {
            scope.insert(surface);
        }
        scope
    }
}

/// SQL 单元绑定源（见 [`crate::ir::BindSrc`]；重导出避免调用方跨模块拼写）。
pub type SqlBindSrc = crate::ir::BindSrc;

// ── M3.5：规则应用器（函数映射；探针见 tests 与 probes） ──────────────

/// token 级重命名 + 命名占位符（一次遍历）。
/// `functions`：`[functions]` 映射；值以 `@` 开头的（@case/@to_number/@to_char）
/// 不在此处理，由 [`apply_ast_transforms`] 解析级完成。
pub fn rewrite_tokens_with_rules(
    sql_text: &str,
    var_names: &VarScope,
    functions: &BTreeMap<String, String>,
) -> Result<SqlUnit, EngineError> {
    let tokens = Tokenizer::new(sql_text)
        .tokenize()
        .map_err(|e| EngineError::invariant(40, format!("tokenizer: {e}")))?;
    let sql_names = collect_sql_names(sql_text, &tokens);
    let mut guard = SqlNameGuard::new(sql_names);
    let mut out = String::with_capacity(sql_text.len() + 64);
    let mut binds: Vec<(String, SqlBindSrc)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = 0usize;
    // 限定名保护：前一个 token 为 '.' 时当前 Ident 是限定名的后半段
    // （table.column / package.routine），不是 PL 变量，禁止替换；
    // 例外：记录变量的限定字段（`rec.field`，rec ∈ 变量集）必须替换为字段绑定
    let mut prev_was_dot = false;
    let mut i = 0usize;
    while i < tokens.len() {
        let tws = &tokens[i];
        let gap = sql_text
            .get(cursor..tws.span.start)
            .ok_or_else(|| EngineError::invariant(41, "token span out of order".to_string()))?;
        out.push_str(gap);
        let action = guard.protect(sql_text, &tokens, i, tws);
        let protected = matches!(action, GuardAction::Protect);
        match &tws.token {
            Token::Ident(value) => {
                let lower = value.to_ascii_lowercase();
                let canon = if prev_was_dot || protected {
                    None
                } else {
                    var_names.resolve_opt(&lower)
                };
                if protected {
                    // SQL 域标识符：原样保留（V60）
                    out.push_str(&sql_text[tws.span.start..tws.span.end]);
                    prev_was_dot = false;
                    cursor = tws.span.end;
                    i += 1;
                    continue;
                }
                if let Some(canon) = canon {
                    // 前瞻 `rec.field`：变量 + '.' + 标识符 → 记录字段绑定
                    if let (Some(dot), Some(field_tok)) = (tokens.get(i + 1), tokens.get(i + 2)) {
                        if matches!(dot.token, Token::Dot)
                            && matches!(field_tok.token, Token::Ident(_))
                        {
                            let Token::Ident(field) = &field_tok.token else {
                                unreachable!("上方已匹配 Ident");
                            };
                            let ph = BindSrc::placeholder(canon, Some(field));
                            out.push_str(&ph);
                            if seen.insert(ph.clone()) {
                                binds.push((
                                    ph,
                                    SqlBindSrc::Field {
                                        rec: VarName(canon.to_string()),
                                        field: field.to_ascii_lowercase(),
                                    },
                                ));
                            }
                            cursor = field_tok.span.end;
                            i += 3;
                            prev_was_dot = false;
                            continue;
                        }
                    }
                    let ph = BindSrc::placeholder(canon, None);
                    out.push_str(&ph);
                    if seen.insert(ph.clone()) {
                        binds.push((ph, SqlBindSrc::Var(VarName(canon.to_string()))));
                    }
                    cursor = tws.span.end;
                    i += 1;
                    continue;
                }
                apply_rename(&mut out, sql_text, tws.span.start, tws.span.end, functions);
            }
            // 关键字（如 SYSDATE）走 Keyword 变体，用源码切片做重命名判定
            Token::Keyword(_) => {
                apply_rename(&mut out, sql_text, tws.span.start, tws.span.end, functions);
            }
            Token::Dot => {
                out.push_str(&sql_text[tws.span.start..tws.span.end]);
                prev_was_dot = true;
                cursor = tws.span.end;
                i += 1;
                continue;
            }
            _ => out.push_str(&sql_text[tws.span.start..tws.span.end]),
        }
        prev_was_dot = false;
        cursor = tws.span.end;
        i += 1;
    }
    out.push_str(&sql_text[cursor..]);
    Ok(SqlUnit {
        text: out,
        binds,
        fn_sites: Vec::new(),
    })
}

/// 括号帧：查询/表达式域标记 + 进入括号前的 FROM 状态快照（`)` 时回卷）。
/// 派生表子查询 `(SELECT … WHERE x = $v_p)` 必须**正常扫描**——组内 PL
/// 变量仍要替换；快照回卷保证内层 WHERE/ORDER 等子句状态不泄漏到外层
/// （`FROM (sub) x, t2` 的逗号分隔表因子依赖外层 in_from）。
struct ParenFrame {
    /// 组内是查询（SELECT/WITH 开头）：组内 FROM 算子句
    query: bool,
    in_from: bool,
    expect_table: bool,
    alias_armed: bool,
}

/// `(` 是否开启**查询域**（组内首个有效词元是 SELECT/WITH）：决定组内的
/// FROM 是否算查询子句 FROM。表达式括号（函数实参/算术组）内的 FROM 属于
/// `EXTRACT(… FROM …)`、`SUBSTRING(… FROM …)`、`TRIM(… FROM …)`、
/// `OVERLAY(… FROM …)` 等函数语法，不是子句。
fn lparen_opens_query(tokens: &[TokenWithSpan], sql_text: &str, i: usize) -> bool {
    for t in tokens[i + 1..].iter() {
        match &t.token {
            Token::Comment(_) | Token::Hint(_) => continue,
            Token::Keyword(_) => {
                let w = &sql_text[t.span.start..t.span.end];
                return w.eq_ignore_ascii_case("SELECT") || w.eq_ignore_ascii_case("WITH");
            }
            _ => return false,
        }
    }
    false
}

/// `FROM` 是否为 `IS [NOT] DISTINCT FROM` 运算符：前一有效词元是 DISTINCT。
/// 该 FROM 后随操作数（常带括号），误当子句 FROM 会让 `expect_table`
/// 把整个 `(expr)` 当子查询组跳过、组内 PL 变量失去替换（fn-site
/// `IS NOT DISTINCT FROM (vk)` 绑定失败回归）。
fn from_is_distinct_operator(tokens: &[TokenWithSpan], sql_text: &str, i: usize) -> bool {
    prev_word_is(tokens, sql_text, i, "DISTINCT")
}

/// `i`（INTO 目标/表名）之后是否紧跟 INSERT 列清单 `(`：允许限定名后缀
/// `.ident` 与注释/提示词元穿插，其余词元一律否决。
fn next_is_into_col_paren(tokens: &[TokenWithSpan], i: usize) -> bool {
    let mut j = i + 1;
    while let Some(t) = tokens.get(j) {
        match &t.token {
            Token::Comment(_) | Token::Hint(_) => j += 1,
            Token::Dot => {
                j += 1;
                match tokens.get(j).map(|t| &t.token) {
                    Some(Token::Ident(_)) | Some(Token::QuotedIdent(_)) => j += 1,
                    _ => return false,
                }
            }
            Token::LParen => return true,
            _ => return false,
        }
    }
    false
}

/// 前一有效词元（跳过注释/提示）是否为指定单词（大小写不敏感）。
fn prev_word_is(tokens: &[TokenWithSpan], sql_text: &str, i: usize, word: &str) -> bool {
    for t in tokens[..i].iter().rev() {
        match &t.token {
            Token::Comment(_) | Token::Hint(_) => continue,
            Token::Keyword(_) | Token::Ident(_) | Token::QuotedIdent(_) => {
                return sql_text[t.span.start..t.span.end].eq_ignore_ascii_case(word);
            }
            _ => return false,
        }
    }
    false
}

/// 简单重命名（同参数序；`@` 前缀的解析级变换在此跳过）。
fn apply_rename(
    out: &mut String,
    sql_text: &str,
    start: usize,
    end: usize,
    functions: &BTreeMap<String, String>,
) {
    let upper = sql_text[start..end].to_ascii_uppercase();
    if let Some(mapped) = functions.get(&upper) {
        if !mapped.starts_with('@') {
            out.push_str(mapped);
            return;
        }
    }
    out.push_str(&sql_text[start..end]);
}

/// 第一遍：收集语句的 **表别名**（含子查询；V60）。
///
/// 名字解析参照 Oracle 语义：SQL 域内的表别名遮蔽外层 PL 同名（探针 G.late——
/// `FROM supplier s` 与 `FOR s IN` 同名时，该句的 `s` 按别名执行——别名在
/// select 列表的限定前缀先于 FROM 出现，故必须先收集后替换）。收集是
/// **语句级**的（每句独立调用），子查询内的别名并入同一集合（跨层抑制是
/// 有意的保守面：同名异层歧义一律归 SQL 域）。表名不做整句抑制——DuckDB
/// 把裸表名解析成整行结构体，`i_item_sk = item` 的参数替换必须保留
/// （C0 守护实测）；表名只在定义位置做位置保护（第二遍）。
fn collect_sql_names(sql_text: &str, tokens: &[TokenWithSpan]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_from = false;
    let mut expect_table = false;
    let mut alias_armed = false;
    // 括号帧栈：query=true 的组内 FROM 算子句；false=表达式括号（函数语法
    // FROM 不算）。帧携带进入前的 FROM 状态，`)` 时回卷（见 ParenFrame）
    let mut paren_q: Vec<ParenFrame> = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        let tws = &tokens[i];
        let word = sql_text[tws.span.start..tws.span.end].to_ascii_uppercase();
        match &tws.token {
            Token::Keyword(_) => match word.as_str() {
                "FROM" => {
                    // 只认查询子句 FROM：DISTINCT FROM 运算符与表达式括号域内
                    // 的函数语法 FROM 不进入 FROM 状态
                    if paren_q.last().map(|f| f.query).unwrap_or(true)
                        && !from_is_distinct_operator(tokens, sql_text, i)
                    {
                        in_from = true;
                        expect_table = true;
                        alias_armed = false;
                    }
                }
                "JOIN" | "UPDATE" | "INTO" => {
                    in_from = true;
                    expect_table = true;
                    alias_armed = false;
                }
                "WHERE" | "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "OFFSET" | "UNION" | "MINUS"
                | "INTERSECT" | "SET" | "VALUES" | "SELECT" | "CONNECT" | "START" | "RETURNING" => {
                    in_from = false;
                    expect_table = false;
                    alias_armed = false;
                }
                "ON" => {
                    expect_table = false;
                    alias_armed = false;
                }
                // `FROM t AS x`：AS 不打断别名位（x 仍按别名登记）
                "AS" => {}
                _ => alias_armed = false,
            },
            Token::Ident(_) | Token::QuotedIdent(_) => {
                if alias_armed && in_from {
                    // 表别名定义：整句抑制（限定前缀在 select 列表先于 FROM 出现）
                    names.insert(word.to_ascii_lowercase());
                    alias_armed = false;
                } else if expect_table {
                    // 表名定义：**不登记**（裸表名在表达式里是整行结构体，参数
                    // 替换必须保留——C0 守护实测），只推进状态等待别名
                    expect_table = false;
                    alias_armed = in_from;
                }
            }
            Token::LParen => {
                if expect_table {
                    // 表因子括号（派生表/表函数）：组后 Ident 仍是外层别名位；
                    // 组内**正常扫描**——别名靠全集/位置规则，PL 变量仍须替换
                    expect_table = false;
                    alias_armed = in_from;
                }
                let query = lparen_opens_query(tokens, sql_text, i);
                paren_q.push(ParenFrame {
                    query,
                    in_from,
                    expect_table,
                    alias_armed,
                });
                in_from = false;
                expect_table = false;
                alias_armed = false;
            }
            Token::RParen => {
                if let Some(f) = paren_q.pop() {
                    in_from = f.in_from;
                    expect_table = f.expect_table;
                    alias_armed = f.alias_armed;
                }
            }
            Token::Comma => {
                if in_from {
                    expect_table = true;
                }
                alias_armed = false;
            }
            _ => alias_armed = false,
        }
        i += 1;
    }
    names
}

/// 第二遍状态机：按第一遍的别名集 + 位置规则，判定词元处置。
/// 位置规则：
/// - 别名（第一遍集合）：整句抑制（限定前缀在 FROM 前出现，必须预收集）；
/// - 表名（FROM/JOIN/UPDATE/INTO 之后的 Ident）：**位置保护**——裸表名在
///   表达式里是整行结构体（DuckDB），替换必须保留（C0 守护实测）；
/// - `AS` 之后的别名定义（单词元；D0/sudf_8：列别名与局部变量同名）；
/// - `INTO 表名 (` 的列清单括号域（X2：列名与循环变量同名）。
enum GuardAction {
    /// SQL 域标识符：原样输出，不替换
    Protect,
    /// 正常扫描
    Scan,
}

struct SqlNameGuard {
    aliases: BTreeSet<String>,
    in_from: bool,
    expect_table: bool,
    alias_armed: bool,
    as_protect: bool,
    into_armed: bool,
    /// INTO 目标是限定名且列清单 `(` 在后：`.ident` 后缀词元不清 into_armed
    into_chain: bool,
    insert_cols: Option<usize>,
    /// 括号帧栈（同 collect_sql_names）：query 帧内 FROM 算子句
    paren_q: Vec<ParenFrame>,
}

impl SqlNameGuard {
    fn new(aliases: BTreeSet<String>) -> Self {
        Self {
            aliases,
            in_from: false,
            expect_table: false,
            alias_armed: false,
            as_protect: false,
            into_armed: false,
            into_chain: false,
            insert_cols: None,
            paren_q: Vec::new(),
        }
    }

    /// 每词元喂一次；返回处置动作。
    fn protect(
        &mut self,
        sql_text: &str,
        tokens: &[TokenWithSpan],
        i: usize,
        tws: &TokenWithSpan,
    ) -> GuardAction {
        let word = sql_text[tws.span.start..tws.span.end].to_ascii_uppercase();
        if let Some(d) = self.insert_cols {
            // INSERT 列清单括号域：一律抑制，深度归零解除
            match &tws.token {
                Token::LParen => self.insert_cols = Some(d + 1),
                Token::RParen => {
                    if d == 1 {
                        self.insert_cols = None;
                    } else {
                        self.insert_cols = Some(d - 1);
                    }
                }
                _ => {}
            }
            return GuardAction::Protect;
        }
        // AS 别名窗口：仅 Ident/QuotedIdent 消费为 Protect，注释/提示穿透，
        // 其余任何词元（含类型 Keyword，如 CAST(x AS DECIMAL) 的 DECIMAL）
        // 都终结窗口——否则保护会武装到下一个无关 Ident（记录变量 r 误保护）
        if self.as_protect && !matches!(&tws.token, Token::Comment(_) | Token::Hint(_)) {
            self.as_protect = false;
            if matches!(&tws.token, Token::Ident(_) | Token::QuotedIdent(_)) {
                return GuardAction::Protect;
            }
        }
        if self.into_chain {
            // INTO 限定名链（`INTO s.t (` 的 `.t`）：链上词元不清 into_armed，
            // 其余词元终结链（列清单 `(` 由 into_armed 分支接走）
            match &tws.token {
                Token::Dot
                | Token::Ident(_)
                | Token::QuotedIdent(_)
                | Token::Comment(_)
                | Token::Hint(_) => return GuardAction::Scan,
                // 列清单 `(` 穿透到主 match 由 into_armed 分支接走
                Token::LParen => {}
                _ => {
                    self.into_chain = false;
                    self.into_armed = false;
                }
            }
        }
        match &tws.token {
            Token::Keyword(_) => {
                match word.as_str() {
                    "AS" => self.as_protect = true,
                    "INTO" => {
                        self.in_from = true;
                        self.expect_table = true;
                        self.alias_armed = false;
                        // 列清单括号域只允许紧跟 INTO 目标/表名的 `(`（见
                        // expect_table 分支的前瞻解除）；SELECT..INTO/FETCH..INTO
                        // 的目标后必有其他词元，不会误挂
                        self.into_armed = true;
                    }
                    "FROM" => {
                        // 只认查询子句 FROM：DISTINCT FROM 运算符与表达式括号域
                        // 内的函数语法 FROM 不进入 FROM 状态（否则 `(expr)` 会被
                        // 当子查询组整体跳过，组内 PL 变量失去替换）
                        if self.paren_q.last().map(|f| f.query).unwrap_or(true)
                            && !from_is_distinct_operator(tokens, sql_text, i)
                        {
                            self.in_from = true;
                            self.expect_table = true;
                            self.alias_armed = false;
                        } else {
                            self.into_armed = false;
                            self.alias_armed = false;
                        }
                    }
                    "JOIN" | "UPDATE" => {
                        self.in_from = true;
                        self.expect_table = true;
                        self.alias_armed = false;
                    }
                    "WHERE" | "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "OFFSET" | "UNION"
                    | "MINUS" | "INTERSECT" | "SET" | "VALUES" | "SELECT" | "CONNECT" | "START"
                    | "RETURNING" => {
                        self.in_from = false;
                        self.expect_table = false;
                        self.alias_armed = false;
                    }
                    "ON" => {
                        self.expect_table = false;
                        self.alias_armed = false;
                    }
                    _ => {
                        self.into_armed = false;
                        self.alias_armed = false;
                    }
                }
                GuardAction::Scan
            }
            Token::Ident(_) | Token::QuotedIdent(_) => {
                if self.aliases.contains(&word.to_ascii_lowercase()) {
                    return GuardAction::Protect;
                }
                if self.expect_table {
                    // 表名：位置保护（裸表名在表达式里是整行结构体，不得替换）。
                    // INSERT 列清单只允许紧跟表名的 `(`（跳过注释词元前瞻）——
                    // 其余后续词元一律解除
                    self.expect_table = false;
                    self.alias_armed = self.in_from;
                    if self.into_armed {
                        if next_is_into_col_paren(tokens, i) {
                            self.into_chain = true;
                        } else {
                            self.into_armed = false;
                        }
                    }
                    return GuardAction::Protect;
                }
                if self.alias_armed && self.in_from {
                    // 第一遍漏网的别名兜底（QuotedIdent 别名等）
                    self.alias_armed = false;
                    return GuardAction::Protect;
                }
                self.into_armed = false;
                GuardAction::Scan
            }
            Token::LParen => {
                if self.into_armed {
                    // INSERT INTO 表 ( 列清单：括号域抑制（对 paren_q 不可见——
                    // 其内部词元提前 Protect 返回，不进出帧栈，天然平衡）
                    self.insert_cols = Some(1);
                    self.into_armed = false;
                    self.into_chain = false;
                    return GuardAction::Protect;
                }
                if self.expect_table {
                    // 表因子括号（派生表/表函数）：组后 Ident 是外层别名位；
                    // 组内**正常扫描**——别名由全集/位置规则保护，组内 PL 变量
                    // 仍须替换（整体跳过会让 `FROM (SELECT … = p_x)` 绑定失败）
                    self.expect_table = false;
                    self.alias_armed = self.in_from;
                }
                let query = lparen_opens_query(tokens, sql_text, i);
                self.paren_q.push(ParenFrame {
                    query,
                    in_from: self.in_from,
                    expect_table: self.expect_table,
                    alias_armed: self.alias_armed,
                });
                self.in_from = false;
                self.expect_table = false;
                self.alias_armed = false;
                self.into_armed = false;
                GuardAction::Scan
            }
            Token::RParen => {
                if let Some(f) = self.paren_q.pop() {
                    self.in_from = f.in_from;
                    self.expect_table = f.expect_table;
                    self.alias_armed = f.alias_armed;
                }
                self.into_armed = false;
                GuardAction::Scan
            }
            Token::Comma => {
                if self.in_from {
                    self.expect_table = true;
                }
                self.into_armed = false;
                self.alias_armed = false;
                GuardAction::Scan
            }
            // 注释/提示对守卫状态透明（`INTO /*c*/ t (cols)`、`FROM t /*c*/ x`）
            Token::Comment(_) | Token::Hint(_) => GuardAction::Scan,
            _ => {
                self.into_armed = false;
                self.alias_armed = false;
                GuardAction::Scan
            }
        }
    }
}
