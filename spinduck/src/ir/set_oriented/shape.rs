//! 完整 token 白名单语法生成静态搬运计划，任何剩余子句均拒绝。

use super::SkipReason;
use ogsql_parser::token::tokenizer::Tokenizer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Name(pub String);

impl Name {
    pub(super) fn sql(&self) -> String {
        format!("\"{}\"", self.0.replace('"', "\"\""))
    }
}

pub(super) struct Select {
    pub table: Name,
    pub alias: Option<Name>,
    /// 输出名（记录字段名），与 `sources` 平行。
    pub columns: Vec<Name>,
    /// 源列名（去限定符），与 `columns` 平行；别名投影时二者不同。
    /// verbatim 游标为恒等映射（输出名即探测列名）。
    pub sources: Vec<Name>,
    /// WHERE 原始 token 文本（小写归一但保留字符串字面量大小写）。
    /// verbatim 游标时仅供全定义证明，渲染已内嵌于 `verbatim` 原文。
    pub filter: Option<String>,
    /// verbatim 游标原文：SELECT 列表含表达式/聚合等不可解析形态时，
    /// 整条查询（含 WHERE/GROUP BY）原文内嵌为 sd_cur，输出列 schema
    /// 由计划期 DESCRIBE 探测。普通游标为 None。
    pub verbatim: Option<String>,
}

impl Select {
    /// 记录字段名（输出名）→ 源列名映射。
    pub(super) fn source_of(&self, out: &Name) -> Option<&Name> {
        self.columns
            .iter()
            .position(|c| c == out)
            .map(|i| &self.sources[i])
    }
}

pub(super) struct Insert {
    pub table: Name,
    pub columns: Vec<Name>,
    pub parameters: Vec<String>,
}

pub(super) struct Tokens {
    words: Vec<String>,
    pos: usize,
}

impl Tokens {
    pub(super) fn new(sql: &str) -> Option<Self> {
        let tokens = Tokenizer::new(sql).tokenize().ok()?;
        let mut words: Vec<String> = tokens
            .iter()
            .filter_map(|t| sql.get(t.span.start..t.span.end))
            .filter(|s| !s.is_empty())
            // 字符串字面量与带引号标识符保留原始大小写（'ES' 与 'es' 语义不同）；
            // 其余 token 小写归一供关键字匹配。
            .map(|s| {
                if s.starts_with('\'') || s.starts_with('"') {
                    s.to_string()
                } else {
                    s.to_ascii_lowercase()
                }
            })
            .collect();
        if words.last().is_some_and(|s| s == ";") {
            words.pop();
        }
        Some(Self { words, pos: 0 })
    }

    pub(super) fn take(&mut self, word: &str) -> bool {
        if self.words.get(self.pos).is_some_and(|w| w == word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn name(&mut self) -> Option<Name> {
        let word = self.words.get(self.pos)?;
        let mut chars = word.chars();
        if !chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        self.pos += 1;
        Some(Name(word.clone()))
    }

    fn names(&mut self) -> Option<Vec<Name>> {
        let mut out = vec![self.name()?];
        while self.take(",") {
            out.push(self.name()?);
        }
        Some(out)
    }

    /// SELECT 投影项：`[限定符 '.'] 源列 [['as'] 别名]`，返回 (限定符, 源列, 输出名)。
    fn select_col(&mut self) -> Option<(Option<Name>, Name, Name)> {
        let first = self.name()?;
        let (qual, src) = if self.take(".") {
            (Some(first), self.name()?)
        } else {
            (None, first)
        };
        let out = if self.take("as") || self.peek().is_some_and(|w| w != "from" && w != ",") {
            self.name()?
        } else {
            src.clone()
        };
        Some((qual, src, out))
    }

    /// 消费剩余全部 token 并以空格连接（WHERE 原文透传）。
    pub(super) fn rest(&mut self) -> String {
        let s = self.words[self.pos..].join(" ");
        self.pos = self.words.len();
        s
    }

    pub(super) fn done(&self) -> bool {
        self.pos == self.words.len()
    }

    pub(super) fn words(&self) -> &[String] {
        &self.words
    }

    pub(super) fn contains(&self, word: &str) -> bool {
        self.words.iter().any(|w| w == word)
    }

    pub(super) fn peek(&self) -> Option<&str> {
        self.words.get(self.pos).map(String::as_str)
    }

    /// 当前 token 为 `(` 且紧随 `select`（标量/缓存子查询起点判定）。
    pub(super) fn at_subquery(&self) -> bool {
        self.words.get(self.pos).is_some_and(|w| w == "(")
            && self.words.get(self.pos + 1).is_some_and(|w| w == "select")
    }

    pub(super) fn number(&mut self) -> Option<String> {
        let word = self.words.get(self.pos)?;
        if word.is_empty() || !word.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        self.pos += 1;
        Some(word.clone())
    }

    /// VALUES 项：收集至顶层 `,`/`)` 的完整表达式 token 串（含嵌套括号）。
    fn expression(&mut self) -> Option<String> {
        let start = self.pos;
        let mut depth = 0usize;
        while let Some(w) = self.words.get(self.pos) {
            match w.as_str() {
                "(" => depth += 1,
                ")" if depth == 0 => break,
                ")" => depth -= 1,
                "," if depth == 0 => break,
                _ => {}
            }
            self.pos += 1;
        }
        (self.pos > start).then(|| self.words[start..self.pos].join(" "))
    }

    /// DEFAULT 子句表达式 token：深度感知消费至顶层 `,`/`)`/`not`（保留后续
    /// NOT NULL 证据）。至少消费一个 token，否则 None。
    pub(super) fn default_tokens(&mut self) -> Option<Vec<String>> {
        let start = self.pos;
        let mut depth = 0usize;
        while let Some(w) = self.words.get(self.pos) {
            match w.as_str() {
                "(" => depth += 1,
                ")" if depth == 0 => break,
                ")" => depth -= 1,
                "," if depth == 0 => break,
                "not" if depth == 0 => break,
                _ => {}
            }
            self.pos += 1;
        }
        (self.pos > start).then(|| self.words[start..self.pos].to_vec())
    }
}

/// 缓存子查询关联列名：`sd_a` + 非空数字后缀。
pub(super) fn is_cache_arg_col(word: &str) -> bool {
    word.strip_prefix("sd_a")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

pub(super) fn select(sql: &str) -> Result<Select, SkipReason> {
    let parse = || {
        let mut t = Tokens::new(sql)?;
        t.take("select").then_some(())?;
        let mut columns = Vec::new();
        let mut sources = Vec::new();
        let mut quals = Vec::new();
        loop {
            let (qual, src, out) = t.select_col()?;
            quals.push(qual);
            sources.push(src);
            columns.push(out);
            if !t.take(",") {
                break;
            }
        }
        t.take("from").then_some(())?;
        let table = t.name()?;
        let alias = if t.take("as") || t.peek().is_some_and(|w| w != "where") {
            t.name()
        } else {
            None
        };
        for q in quals.iter().flatten() {
            let ok = match &alias {
                Some(a) => q == a,
                None => q == &table,
            };
            if !ok {
                return None;
            }
        }
        let filter = if t.take("where") {
            Some(t.rest())
        } else {
            None
        };
        t.done().then_some(Select {
            table,
            alias,
            columns,
            sources,
            filter,
            verbatim: None,
        })
    };
    parse().ok_or(SkipReason::UnprovenExpression)
}

/// verbatim 游标解析：SELECT 列表逐项取输出名（`AS 别名` 或裸/限定
/// 列名），FROM 限定为单表 + 可选别名，WHERE（可选）提取至顶层
/// GROUP/ORDER/LIMIT/HAVING 为止供全定义证明；其余子句随原文透传。
/// JOIN/逗号多源、无法定名的投影项一律拒绝。
pub(super) fn select_verbatim(sql: &str) -> Result<Select, SkipReason> {
    let bad = SkipReason::UnprovenExpression;
    let t = Tokens::new(sql).ok_or(bad)?;
    let w = t.words();
    if w.first().map(String::as_str) != Some("select") {
        return Err(bad);
    }
    // 投影项：顶层 `,` 分隔，顶层 from/group/order/limit/having 收尾。
    let mut items: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut depth = 0i32;
    let mut i = 1;
    let mut tail = None;
    while i < w.len() {
        match w[i].as_str() {
            "(" => {
                depth += 1;
                cur.push(w[i].as_str());
            }
            ")" => {
                depth -= 1;
                if depth < 0 {
                    return Err(bad);
                }
                cur.push(w[i].as_str());
            }
            "," if depth == 0 => items.push(std::mem::take(&mut cur)),
            "from" | "group" | "order" | "limit" | "having" if depth == 0 => {
                tail = Some((w[i].as_str(), i));
                break;
            }
            _ => cur.push(w[i].as_str()),
        }
        i += 1;
    }
    if !cur.is_empty() {
        items.push(cur);
    }
    let Some(("from", from_at)) = tail else {
        return Err(bad);
    };
    let mut columns = Vec::with_capacity(items.len());
    for item in &items {
        let out = verbatim_out_name(item).ok_or(bad)?;
        if columns.contains(&out) {
            return Err(bad);
        }
        columns.push(out);
    }
    if columns.is_empty() {
        return Err(bad);
    }
    // FROM：单表 + 可选别名；其后只允许 WHERE 或尾部子句（拒绝 JOIN/逗号）。
    let mut j = from_at + 1;
    let table = ident_name(w.get(j).ok_or(bad)?).ok_or(bad)?;
    j += 1;
    let clause_start = |s: &str| {
        matches!(
            s,
            "where" | "group" | "order" | "limit" | "having" | "," | ";"
        )
    };
    let alias = if w.get(j).is_some_and(|x| x == "as") {
        j += 1;
        let a = ident_name(w.get(j).ok_or(bad)?).ok_or(bad)?;
        j += 1;
        Some(a)
    } else if w
        .get(j)
        .is_some_and(|x| !clause_start(x) && ident_name(x).is_some())
    {
        let a = ident_name(w.get(j).ok_or(bad)?).ok_or(bad)?;
        j += 1;
        Some(a)
    } else {
        None
    };
    if w.get(j).is_some_and(|x| !clause_start(x)) {
        return Err(bad);
    }
    // WHERE：提取至顶层 GROUP/ORDER/LIMIT/HAVING（含 HAVING 之后不取）。
    let filter = if w.get(j).is_some_and(|x| x == "where") {
        j += 1;
        let start = j;
        let mut d = 0i32;
        while j < w.len() {
            match w[j].as_str() {
                "(" => d += 1,
                ")" => d -= 1,
                "group" | "order" | "limit" | "having" if d == 0 => break,
                _ => {}
            }
            j += 1;
        }
        Some(w[start..j].join(" "))
    } else {
        None
    };
    Ok(Select {
        table,
        alias,
        sources: columns.clone(),
        columns,
        filter,
        verbatim: Some(sql.to_string()),
    })
}

/// verbatim 投影项输出名：顶层 `AS 名` 优先；否则仅 `[限定 '.'] 列名`
/// 单形态可定名（表达式无别名时 DuckDB 命名不可证，拒绝）。
fn verbatim_out_name(item: &[&str]) -> Option<Name> {
    let mut depth = 0i32;
    for (i, w) in item.iter().enumerate() {
        match *w {
            "(" => depth += 1,
            ")" => depth -= 1,
            "as" if depth == 0 => return ident_name(item.get(i + 1)?),
            _ => {}
        }
    }
    match item {
        [n] => ident_name(n),
        [_, ".", n] => ident_name(n),
        _ => None,
    }
}

/// 与 `Tokens::name` 同口径的标识符判定（不消费游标）。
fn ident_name(word: &str) -> Option<Name> {
    let mut chars = word.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(Name(word.to_string()))
}

/// 普通游标的 WHERE 是否残留顶层 GROUP/ORDER/LIMIT/HAVING 子句
/// （`shape::select` 的 `rest()` 会把它们并入 filter 文本，此时应走
/// verbatim 路径而非让全定义证明误判）。
pub(super) fn filter_has_tail(filter: &str) -> bool {
    let Some(t) = Tokens::new(filter) else {
        return true;
    };
    let mut depth = 0i32;
    t.words().iter().any(|w| {
        let hit = depth == 0 && matches!(w.as_str(), "group" | "order" | "limit" | "having");
        match w.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            _ => {}
        }
        hit
    })
}

/// DESCRIBE 探测用改写：`$v_` 绑定占位符与 fn_site 参数缓存子查询
/// `(SELECT sd_v FROM sd_fnN WHERE …)`（含 `(SELECT (…))` 标量包装的
/// 内层）替换为 NULL——探测只取输出列类型，WHERE/实参不影响投影类型。
/// 无法词法化时返回 None（调用方按不可证处理）。
pub(super) fn probe_sql(sql: &str) -> Option<String> {
    let t = Tokens::new(sql)?;
    let w = t.words();
    let mut out: Vec<String> = Vec::with_capacity(w.len());
    let mut i = 0;
    while i < w.len() {
        if w[i].starts_with("$v_") {
            out.push("NULL".to_string());
            i += 1;
            continue;
        }
        if w[i] == "("
            && w.get(i + 1).is_some_and(|x| x == "select")
            && w.get(i + 2).is_some_and(|x| x == "sd_v")
            && w.get(i + 3).is_some_and(|x| x == "from")
            && w.get(i + 4).is_some_and(|x| x.starts_with("sd_fn"))
        {
            let mut depth = 0i32;
            let mut j = i;
            while j < w.len() {
                match w[j].as_str() {
                    "(" => depth += 1,
                    ")" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if j >= w.len() {
                return None;
            }
            out.push("NULL".to_string());
            i = j + 1;
            continue;
        }
        out.push(w[i].clone());
        i += 1;
    }
    Some(out.join(" "))
}

pub(super) fn insert(sql: &str) -> Result<Insert, SkipReason> {
    let parse = || {
        let mut t = Tokens::new(sql)?;
        (t.take("insert") && t.take("into")).then_some(())?;
        let table = t.name()?;
        t.take("(").then_some(())?;
        let columns = t.names()?;
        (t.take(")") && t.take("values") && t.take("(")).then_some(())?;
        let mut parameters = vec![t.expression()?];
        while t.take(",") {
            parameters.push(t.expression()?);
        }
        (t.take(")") && t.done()).then_some(Insert {
            table,
            columns,
            parameters,
        })
    };
    parse().ok_or(SkipReason::UnsupportedWrite)
}
