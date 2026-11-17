//! `.sdsh` 批处理脚本词法层：逐行 → 指令 token 序列。
//!
//! 词法契约（设计 §2.2/§2.2.6）：
//! - `#` 起始的整行与空行跳过；无行内注释（`#` 可能出现在合法参数里）。
//! - `"..."` 剥引号且展开 `$VAR`/`${VAR}`；`'...'` 剥引号且原样禁展开；
//!   裸词同样展开。`\$`/`\\` 转义。
//! - 未定义变量（`vars` 与进程环境均无）→ `ENV-0017` Fail-Fast。
//! - Shell 算符（`| > >> < << && || ; &`）只在**裸词 token** 上判定——
//!   `--where "a;b"`、`--slice 'x|y'` 的引号内容不算。
//! - `str:` 前缀仅在 `call` 行的非旗标位置 token 上生效（→ `'...'` 字面量，
//!   供 `parse_literal` 保真前导零）；旗标值（紧跟 `--xxx`）不转换。
//! - 禁入指令：`batch`、`help`（含 `--help`/`-h` 首词）。

use crate::error::EngineError;
use std::collections::BTreeMap;

/// 解析后的脚本行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchLine {
    /// `use <库名>`：更新默认库（`:memory:` = 显式共享内存库）。
    Use(String),
    /// 顶层 `--db <路径>`：等价 `use`，仅写法不同。
    GlobalDb(String),
    /// `set KEY [=] VALUE`：批内变量（值为剩余 token 以单空格连接）。
    Set { key: String, value: String },
    /// 普通子命令：首 token = 子命令名，其余为已处理参数 token。
    Command(Vec<String>),
}

/// 脚本解析产物：指令序列 + `use`/`--db` 推演后的最终默认库。
#[derive(Debug)]
pub struct ParsedScript {
    pub lines: Vec<BatchLine>,
    pub default_db: Option<String>,
}

fn env_err(code: u16, msg: impl std::fmt::Display) -> EngineError {
    EngineError::env(code, msg.to_string())
}

fn usage_err(msg: impl std::fmt::Display) -> EngineError {
    EngineError::invariant(10, format!("sdsh: {msg}"))
}

/// 整脚本词法解析。`set` 指令的变量在行内累积并供后续行展开。
/// 物理行先经 [`logical_lines`] 拼接续行（行尾引号外奇数个 `\`），
/// 再逐逻辑行切词——设计 §2.2 的换行续行契约。
pub fn parse_script(text: &str) -> Result<ParsedScript, EngineError> {
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    let mut lines = Vec::new();
    let mut default_db: Option<String> = None;
    for (line_no, raw) in logical_lines(text)? {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let tokens = lex_tokens(trimmed, &vars, line_no)?;
        if tokens.is_empty() {
            continue;
        }
        let head = tokens[0].as_str();
        match head {
            "batch" | "help" | "--help" | "-h" => {
                return Err(usage_err(format!(
                    "line {line_no}: `{head}` 不允许嵌套/进入 batch"
                )));
            }
            "use" => {
                if tokens.len() != 2 {
                    return Err(usage_err(format!("line {line_no}: use <库名>")));
                }
                default_db = Some(tokens[1].clone());
                lines.push(BatchLine::Use(tokens[1].clone()));
            }
            "--db" => {
                if tokens.len() != 2 {
                    return Err(usage_err(format!("line {line_no}: --db <路径>")));
                }
                default_db = Some(tokens[1].clone());
                lines.push(BatchLine::GlobalDb(tokens[1].clone()));
            }
            "set" => {
                if tokens.len() < 3 {
                    return Err(usage_err(format!("line {line_no}: set <KEY> [=] <VALUE>")));
                }
                let key = tokens[1].clone();
                let rest = &tokens[2..];
                let rest = if rest.first().map(|s| s.as_str()) == Some("=") {
                    &rest[1..]
                } else {
                    rest
                };
                if rest.is_empty() {
                    return Err(usage_err(format!("line {line_no}: set {key} 缺值")));
                }
                let value = rest.join(" ");
                vars.insert(key.clone(), value.clone());
                lines.push(BatchLine::Set { key, value });
            }
            _ => {
                check_shell_ops(&tokens, line_no)?;
                let tokens = if head == "call" {
                    apply_str_sugar(tokens)
                } else {
                    tokens
                };
                lines.push(BatchLine::Command(tokens));
            }
        }
    }
    Ok(ParsedScript { lines, default_db })
}

/// 物理行 → 逻辑行（行号取续行首行的物理行号）。
/// 续行规则（设计 §2.2/§4.1）：**换行符紧邻**的引号外奇数个 `\` 才续行——
/// `\` 与换行之间夹空白不续行；`\\`（偶数个）是字面反斜杠；引号未闭合的行
/// 不续行（交给词法层报引号错）；整行 `#` 注释与空行不进入续行拼接
/// （注释词不得成为 argv），且会终结进行中的续行；EOF 处仍挂续行 →
/// `ENG-0010`（孤立 `\` 是词法错误，不是静默丢字符）。
fn logical_lines(text: &str) -> Result<Vec<(usize, String)>, EngineError> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw.trim();
        let blank_or_comment = trimmed.is_empty() || trimmed.starts_with('#');
        if let Some((start, acc)) = pending.take() {
            if blank_or_comment {
                // 注释/空行不写入逻辑行；续行就此完结（\ 换行已被消耗）
                out.push((start, acc));
                continue;
            }
            // 拼接处补一个空格：`foo\`+`bar` 不得黏成 `foobar`
            let mut logical = acc;
            logical.push(' ');
            logical.push_str(raw);
            match continuation_cut(&logical) {
                Some(cut) => pending = Some((start, logical[..cut].to_string())),
                None => out.push((start, logical)),
            }
            continue;
        }
        if blank_or_comment {
            continue;
        }
        let mut logical = String::new();
        logical.push_str(raw);
        match continuation_cut(&logical) {
            Some(cut) => pending = Some((line_no, logical[..cut].to_string())),
            None => out.push((line_no, logical)),
        }
    }
    if pending.is_some() {
        return Err(usage_err("行尾孤立 `\\`（续行符后没有下一行）"));
    }
    Ok(out)
}

/// 若逻辑行以「引号外的奇数个 `\`」结尾，返回该续行反斜杠的字节下标。
/// 只看最后一个字符：`\` 后还有空白则不是续行（交给词法层按转义处理）。
fn continuation_cut(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let (mut i, mut in_s, mut in_d) = (0usize, false, false);
    while i < bytes.len() {
        let b = bytes[i];
        if in_d {
            if b == b'\\' {
                i += 1; // 跳过被转义字符（含 \" 不闭合引号）
            } else if b == b'"' {
                in_d = false;
            }
        } else if in_s {
            if b == b'\'' {
                in_s = false;
            }
        } else {
            match b {
                b'"' => in_d = true,
                b'\'' => in_s = true,
                _ => {}
            }
        }
        i += 1;
    }
    if in_s || in_d || bytes.last() != Some(&b'\\') {
        return None;
    }
    let mut bs = 0usize;
    let mut j = bytes.len();
    while j > 0 && bytes[j - 1] == b'\\' {
        bs += 1;
        j -= 1;
    }
    (bs % 2 == 1).then_some(bytes.len() - 1)
}

/// token 的词法片段：`Lit` = 引号/转义产出的最终文本（不再展开）；
/// `Bare` = 裸词片段（收尾时统一做 `$VAR` 展开）。分段记录避免引号内已
/// 展开值里的 `$` 被二次展开。
#[derive(Debug)]
enum Seg {
    Lit(String),
    Bare(String),
}

struct TokenBuf {
    segs: Vec<Seg>,
    bare: String,
    started: bool,
}

impl TokenBuf {
    fn new() -> Self {
        Self {
            segs: Vec::new(),
            bare: String::new(),
            started: false,
        }
    }
    fn push_bare(&mut self, c: char) {
        self.started = true;
        self.bare.push(c);
    }
    fn push_lit(&mut self, s: String) {
        self.started = true;
        if !self.bare.is_empty() {
            self.segs.push(Seg::Bare(std::mem::take(&mut self.bare)));
        }
        self.segs.push(Seg::Lit(s));
    }
    /// token 收尾：裸词片段并入 segs，逐段展开/直通后拼接。
    fn finish(
        self,
        vars: &BTreeMap<String, String>,
        line_no: usize,
    ) -> Result<String, EngineError> {
        let mut out = String::new();
        let mut segs = self.segs;
        if !self.bare.is_empty() {
            segs.push(Seg::Bare(self.bare));
        }
        for seg in segs {
            match seg {
                Seg::Lit(s) => out.push_str(&s),
                Seg::Bare(s) => out.push_str(&expand_vars(&s, vars, line_no)?),
            }
        }
        Ok(out)
    }
}

/// 单行 → token 序列（引号剥离 + 变量展开 + `\$`/`\\` 转义）。
fn lex_tokens(
    line: &str,
    vars: &BTreeMap<String, String>,
    line_no: usize,
) -> Result<Vec<String>, EngineError> {
    let mut tokens = Vec::new();
    let mut cur = TokenBuf::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if cur.started {
                    tokens.push(cur.finish(vars, line_no)?);
                    cur = TokenBuf::new();
                }
            }
            '"' => cur.push_lit(scan_quoted(&mut chars, '"', vars, line_no, true)?),
            '\'' => cur.push_lit(scan_quoted(&mut chars, '\'', vars, line_no, false)?),
            '\\' => match chars.next() {
                Some('$') => cur.push_bare('$'),
                Some('\\') => cur.push_bare('\\'),
                Some(other) => {
                    cur.push_bare('\\');
                    cur.push_bare(other);
                }
                None => cur.push_bare('\\'),
            },
            _ => cur.push_bare(c),
        }
    }
    if cur.started {
        tokens.push(cur.finish(vars, line_no)?);
    }
    Ok(tokens)
}

/// 引号内容扫描：返回引号内的最终文本（`expand` = 双引号 true / 单引号 false；
/// 双引号内 `$VAR` 即时展开、`\$`/`\\` 转义；单引号内全部字面）。不闭合 → 用法错。
fn scan_quoted(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    quote: char,
    vars: &BTreeMap<String, String>,
    line_no: usize,
    expand: bool,
) -> Result<String, EngineError> {
    let mut seg = String::new();
    loop {
        match chars.next() {
            Some(c) if c == quote => break,
            Some('\\') if quote == '"' => match chars.next() {
                Some('$') => seg.push('$'),
                Some('\\') => seg.push('\\'),
                Some('"') => seg.push('"'),
                Some(other) => {
                    seg.push('\\');
                    seg.push(other);
                }
                None => seg.push('\\'),
            },
            Some('$') if expand => {
                seg.push('$');
                collect_var_name(chars, &mut seg);
            }
            Some(c) => seg.push(c),
            None => return Err(usage_err(format!("line {line_no}: 引号未闭合（{quote}）"))),
        }
    }
    if expand {
        expand_vars(&seg, vars, line_no)
    } else {
        Ok(seg)
    }
}

/// `$` 后的变量名读入 `seg`（`${...}` 或 `$IDENT`）。
fn collect_var_name(chars: &mut std::iter::Peekable<std::str::Chars>, seg: &mut String) {
    if chars.peek() == Some(&'{') {
        seg.push('{');
        for c in chars.by_ref() {
            seg.push(c);
            if c == '}' {
                break;
            }
        }
        return;
    }
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphanumeric() || c == '_' {
            seg.push(c);
            chars.next();
        } else {
            break;
        }
    }
}

/// 对文本中的 `$KEY`/`${KEY}` 做 vars → 进程 env 两级查找；缺失 → ENV-0017。
fn expand_vars(
    text: &str,
    vars: &BTreeMap<String, String>,
    line_no: usize,
) -> Result<String, EngineError> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        let mut braced = false;
        if chars.peek() == Some(&'{') {
            chars.next();
            braced = true;
            for c2 in chars.by_ref() {
                if c2 == '}' {
                    break;
                }
                name.push(c2);
            }
        } else {
            while let Some(&c2) = chars.peek() {
                if c2.is_ascii_alphanumeric() || c2 == '_' {
                    name.push(c2);
                    chars.next();
                } else {
                    break;
                }
            }
        }
        if name.is_empty() {
            out.push('$');
            if braced {
                out.push('}');
            }
            continue;
        }
        let val = vars
            .get(&name)
            .cloned()
            .or_else(|| std::env::var(&name).ok());
        match val {
            Some(v) => out.push_str(&v),
            None => {
                return Err(env_err(
                    17,
                    format!("line {line_no}: 未定义变量 ${name}（vars 与进程环境均无）"),
                ))
            }
        }
    }
    Ok(out)
}

/// Shell 算符只在裸词 token 上判定（引号内容在词法期已并入 token 字符串，
/// 形如 `a;b` 的整体 token 不等于裸 `;`，天然不误杀）。
fn check_shell_ops(tokens: &[String], line_no: usize) -> Result<(), EngineError> {
    const OPS: [&str; 9] = ["|", ">", ">>", "<", "<<", "&&", "||", ";", "&"];
    for t in tokens {
        if OPS.contains(&t.as_str()) {
            return Err(usage_err(format!(
                "line {line_no}: 禁止 shell 算符 `{t}`（batch 内不支持管道/重定向/链式）"
            )));
        }
    }
    Ok(())
}

/// `call` 行的 `str:` 语法糖：非旗标位置 token `str:X` → `'X'`（`parse_literal`
/// 的单引号形态 → `SpinValue::Str`，保真前导零）。
/// 旗标值判定必须与 `parse_call` 的取值规则一致：**吃下一个 token 的旗标**
/// 才跳过 `str:` 转换；`--batch-commit` 是无值旗标，其后的 token 仍是位置
/// 参数（按 `--` 前缀一刀切会把 `--batch-commit str:x` 的
/// `str:x` 漏转换、把 `--file` 的值误转）。
const CALL_VALUE_FLAGS: [&str; 8] = [
    "--db",
    "--rules",
    "--file",
    "--step-budget",
    "--trace",
    "--trace-format",
    "--trace-level",
    "--plan",
];

fn apply_str_sugar(tokens: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut prev_is_flag_value = false;
    for t in tokens {
        if prev_is_flag_value {
            out.push(t);
            prev_is_flag_value = false;
            continue;
        }
        if let Some(rest) = t.strip_prefix("str:") {
            if !rest.is_empty() {
                out.push(format!("'{rest}'"));
                prev_is_flag_value = false;
                continue;
            }
        }
        prev_is_flag_value = CALL_VALUE_FLAGS.contains(&t.as_str());
        out.push(t);
    }
    out
}

/// 显式 `:memory:` 库 spec 判定（共享内存库语义；省略 `--db` 的匿名内存库
/// 不走这里——那是每步独立的 `open_in_memory`）。
pub fn is_memory_spec(spec: &str) -> bool {
    spec == ":memory:"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(text: &str) -> Vec<Vec<String>> {
        parse_script(text)
            .unwrap()
            .lines
            .into_iter()
            .filter_map(|l| match l {
                BatchLine::Command(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn should_strip_quotes_and_expand_vars_in_double_quotes() {
        std::env::set_var("SDDB", "w.duckdb");
        let got = cmds("query --db \"$SDDB\" --sql-file 'q.sql' --out o.json\n");
        assert_eq!(
            got,
            vec![vec![
                "query",
                "--db",
                "w.duckdb",
                "--sql-file",
                "q.sql",
                "--out",
                "o.json"
            ]]
        );
    }

    #[test]
    fn should_keep_single_quotes_literal_without_expansion() {
        std::env::set_var("SDV", "EXPANDED");
        let got = cmds("call --file f.sql prc 'bcrq=$SDV'\n");
        assert_eq!(got[0][4], "bcrq=$SDV");
        let got2 = cmds("call --file f.sql prc \"bcrq=$SDV\"\n");
        assert_eq!(got2[0][4], "bcrq=EXPANDED");
    }

    #[test]
    fn should_reject_undefined_variable_with_env_0017() {
        let e = parse_script("query --db \"$NOPE_XYZ\" --sql-file q --out o\n").unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0017", "{e}");
    }

    #[test]
    fn should_expand_set_vars_in_later_lines() {
        let got = cmds("set T = my_table\nquery --db w --sql-file q.sql --out \"$T.json\"\n");
        assert_eq!(got[0][6], "my_table.json");
    }

    #[test]
    fn should_reject_bare_shell_operators_only() {
        assert!(parse_script("call --file f prc 1 ; rm -rf\n").is_err());
        assert!(parse_script("call --file f prc 1 | cat\n").is_err());
        // 引号内的算符是数据不是算符
        assert!(parse_script("sync --db w --table t --cols a --where \"x>1;y\"\n").is_ok());
        assert!(
            parse_script("sync --db w --table t --cols a --slice 'k=a|b' --mode slice\n").is_ok()
        );
    }

    #[test]
    fn should_apply_str_sugar_only_on_call_positional_args() {
        let got = cmds("call --file f.sql prc_calc str:0130000001 14\n");
        assert_eq!(got[0][4], "'0130000001'");
        // 旗标值不转换
        let got2 = cmds("call --file str:x.sql prc 1\n");
        assert_eq!(got2[0][2], "str:x.sql");
        // 非 call 行不转换
        let got3 = cmds("query --db w --sql-file str:q.sql --out o\n");
        assert_eq!(got3[0][4], "str:q.sql");
    }

    #[test]
    fn should_apply_str_sugar_after_valueless_batch_commit_flag() {
        // --batch-commit 不吃下一个 token，其后的 str: 仍是位置实参
        let got = cmds("call --file f.sql prc_id --batch-commit str:0130000001\n");
        assert_eq!(got[0][5], "'0130000001'", "{got:?}");
        // 无值旗标后紧跟吃值旗标：str: 是 --file 的值，不得转换
        let got2 = cmds("call prc --batch-commit --file str:notes.sql\n");
        assert_eq!(got2[0][4], "str:notes.sql", "{got2:?}");
    }

    #[test]
    fn should_track_use_and_global_db() {
        let s = parse_script("use :memory:\n--db other.duckdb\ncall --file f prc\n").unwrap();
        assert_eq!(s.default_db.as_deref(), Some("other.duckdb"));
        assert!(matches!(s.lines[0], BatchLine::Use(_)));
        assert!(matches!(s.lines[1], BatchLine::GlobalDb(_)));
    }

    #[test]
    fn should_reject_nested_batch_and_help() {
        assert!(parse_script("batch other.sdsh\n").is_err());
        assert!(parse_script("help\n").is_err());
    }

    #[test]
    fn should_not_double_expand_quoted_values_containing_dollar() {
        // 引号内已展开的值若含 $，不得被裸词二次展开（分段词法的回归守护）
        std::env::set_var("SDINNER", "$SDOUTER");
        std::env::set_var("SDOUTER", "LEAK");
        let got = cmds("call --file f prc \"pre$SDINNER\"post\n");
        assert_eq!(got[0][4], "pre$SDOUTERpost");
    }

    #[test]
    fn should_join_backslash_continuation_lines() {
        // 设计 §2.2 续行：行尾引号外奇数个 \ 拼接下一物理行
        let got = cmds("materialize --db w --table t \\\n  --jobs 2\n");
        assert_eq!(
            got[0],
            vec!["materialize", "--db", "w", "--table", "t", "--jobs", "2"]
        );
        // 偶数个 \\ 是字面反斜杠，不续行（下一行独立成指令）
        let got2 = cmds("call --file f prc a\\\\\ncall --file f prc b\n");
        assert_eq!(got2.len(), 2, "{got2:?}");
        // 引号内的 \" 不闭合引号：该行引号未闭合，词法层报错而非续行
        assert!(parse_script("call --file f prc \"x\\\" \ncall --file f prc y\n").is_err());
    }

    #[test]
    fn should_unescape_quote_inside_double_quotes() {
        // 设计 §2.2：双引号内 \" → "
        let got = cmds("call --file f prc \"a=\\\"b\\\"\"\n");
        assert_eq!(got[0][4], "a=\"b\"", "{got:?}");
    }

    #[test]
    fn should_reject_lone_trailing_backslash_at_eof() {
        // 孤立续行符是词法错误（ENG-0010），不得静默吞掉 \ 继续执行
        let e = parse_script("call --file f prc str:013\\\n").unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010", "{e}");
        let e2 = parse_script("call --file f prc str:013\\").unwrap_err();
        assert_eq!(e2.code.to_string(), "ENG-0010", "{e2}");
    }

    #[test]
    fn should_not_continue_over_comment_or_blank_lines() {
        // 续行遇整行 # 注释/空行：注释词不得进入 argv，续行就此完结
        let got = cmds("call --file f prc \\\n  # note\nstr:1\n");
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0], vec!["call", "--file", "f", "prc"]);
        assert_eq!(got[1], vec!["str:1"]);
        // \ 与换行之间夹空白不是续行：两行各自成指令
        let got2 = cmds("call --file f prc a\\ \ncall --file f prc b\n");
        assert_eq!(got2.len(), 2, "{got2:?}");
    }
}
