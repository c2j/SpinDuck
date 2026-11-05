//! 括号配平 + 引号感知的函数调用扫描（供解析级变换原位替换）。

/// 括号配平 + 引号感知的函数调用扫描：返回 (名称起点, 右括号终点, 名称, 参数文本)。
/// 只返回"最内层"调用（参数文本中不再包含待变换函数名），保证拼接正确。
pub(super) fn find_innermost_calls(
    text: &str,
    names: &[&str],
) -> Vec<(usize, usize, String, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut in_string = false;
    while i < text.len() {
        let c = bytes[i] as char;
        if in_string {
            if c == '\'' {
                if i + 1 < text.len() && bytes[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_string = false;
                }
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_string = true;
                i += 1;
            }
            _ => {
                if c.is_ascii_alphabetic() || c == '_' {
                    let start = i;
                    while i < text.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                        i += 1;
                    }
                    let word = &text[start..i];
                    let upper = word.to_ascii_uppercase();
                    // 名字与 '(' 之间允许空白（格式化器产出 `decode (` 形态）
                    let mut k = i;
                    while k < text.len() && (bytes[k] as char).is_whitespace() {
                        k += 1;
                    }
                    if names.contains(&upper.as_str()) && k < text.len() && bytes[k] == b'(' {
                        // 配平右括号
                        let mut depth = 0usize;
                        let mut j = k;
                        let mut s2 = in_string;
                        while j < text.len() {
                            let ch = bytes[j] as char;
                            if s2 {
                                if ch == '\'' {
                                    if j + 1 < text.len() && bytes[j + 1] == b'\'' {
                                        j += 1;
                                    } else {
                                        s2 = false;
                                    }
                                }
                            } else {
                                match ch {
                                    '\'' => s2 = true,
                                    '(' => depth += 1,
                                    ')' => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            j += 1;
                        }
                        // 括号未配平（含未闭合字符串吞掉右括号）→ 不识别该调用，
                        // 交由下游显式报错；不得产出越界区间（回归守护见
                        // should_not_panic_on_unterminated_string）
                        if depth == 0 && j < text.len() {
                            let args = &text[k + 1..j];
                            let has_nested = names.iter().any(|n| upper_eq_find(args, n));
                            if !has_nested {
                                out.push((start, j + 1, upper, args.to_string()));
                            }
                        }
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    out
}

pub(super) fn upper_eq_find(haystack: &str, name: &str) -> bool {
    let up = haystack.to_ascii_uppercase();
    let pat = format!("{name}(");
    up.contains(&pat)
}
