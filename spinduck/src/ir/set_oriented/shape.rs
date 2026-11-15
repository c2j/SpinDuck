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
    pub columns: Vec<Name>,
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
            .map(|s| s.to_ascii_lowercase())
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

    fn parameter(&mut self) -> Option<String> {
        let word = self.words.get(self.pos)?.clone();
        if !word.starts_with("$v_")
            || !word[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        self.pos += 1;
        Some(word)
    }

    pub(super) fn done(&self) -> bool {
        self.pos == self.words.len()
    }
}

pub(super) fn select(sql: &str) -> Result<Select, SkipReason> {
    let parse = || {
        let mut t = Tokens::new(sql)?;
        t.take("select").then_some(())?;
        let columns = t.names()?;
        t.take("from").then_some(())?;
        let table = t.name()?;
        t.done().then_some(Select { table, columns })
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
        let mut parameters = vec![t.parameter()?];
        while t.take(",") {
            parameters.push(t.parameter()?);
        }
        (t.take(")") && t.done()).then_some(Insert {
            table,
            columns,
            parameters,
        })
    };
    parse().ok_or(SkipReason::UnsupportedWrite)
}
