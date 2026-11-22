//! 查找文法 token 流：保留字符串字面量原大小写，关键字/标识符小写归一。

use super::super::Name;

/// 保留字符串字面量原大小写的 token 流（关键字/标识符仍小写归一）。
pub(super) struct LiteralTokens {
    pub(super) words: Vec<String>,
    pub(super) pos: usize,
}

impl LiteralTokens {
    pub(super) fn new(sql: &str) -> Option<Self> {
        use ogsql_parser::token::tokenizer::Tokenizer;
        let tokens = Tokenizer::new(sql).tokenize().ok()?;
        let mut words: Vec<String> = tokens
            .iter()
            .filter_map(|t| sql.get(t.span.start..t.span.end))
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.starts_with('\'') {
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

    pub(super) fn peek(&self) -> Option<&str> {
        self.words.get(self.pos).map(String::as_str)
    }

    pub(super) fn done(&self) -> bool {
        self.pos == self.words.len()
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

    /// 列名：可选 `别名.列` 限定（单表，前缀丢弃），返回裸列名。
    pub(super) fn column(&mut self) -> Option<Name> {
        let first = self.name()?;
        if self.take(".") {
            self.name()
        } else {
            Some(first)
        }
    }

    /// 消费可选表别名（FROM 表名之后、WHERE/ORDER/结束之前的 `[AS] 标识符`）。
    pub(super) fn skip_alias(&mut self) {
        self.take("as");
        let is_alias = self.peek().is_some_and(|w| {
            !matches!(
                w,
                "where" | "order" | "limit" | "group" | "having" | "union" | "intersect" | "except"
            ) && w
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        });
        if is_alias {
            self.name();
        }
    }

    /// 字符串（单引号包围）或数字字面量，原样返回。
    pub(super) fn literal(&mut self) -> Option<String> {
        let word = self.words.get(self.pos)?;
        let ok = (word.starts_with('\'') && word.ends_with('\'') && word.len() >= 2)
            || (!word.is_empty()
                && word.chars().all(|c| c.is_ascii_digit() || c == '.')
                && word.chars().any(|c| c.is_ascii_digit()));
        if !ok {
            return None;
        }
        self.pos += 1;
        Some(word.clone())
    }
}
