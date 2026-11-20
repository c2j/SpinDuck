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
    pub sources: Vec<Name>,
    /// WHERE 原始 token 文本（小写归一但保留字符串字面量大小写）。
    pub filter: Option<String>,
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
        })
    };
    parse().ok_or(SkipReason::UnprovenExpression)
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
