//! SQL 源文件的读入与编码判定（design §3.5 / D9②）。
//!
//! 判定顺序固定为 **UTF-8 严格 → GBK 严格 → 红**，两条都不成立就拒绝，绝不退回有损解码：
//! D9① 消灭的静默形态正是"`from_utf8_lossy` 把中文字面量改成 U+FFFD 却 rc=0"。
//! UTF-8 优先是判定不是风格：交付件里 5,130 个文件的字节两种编码都合法，按 GBK 读必成乱码。
//! 转码只发生在**读入时**，语料文件逐字不动（AGENTS §6 语料纪律）。
//!
//! 消费面：`call`/`explain`/`deploy`/`query`/`compare`（经 `cli::read_source_strict`）与
//! 部署链路的四处文件下发（表 DDL、视图、例程注册、数据集种子）共用这里，两侧不得各有
//! 一份判定。唯一例外是 `compile` 体检面：它要"一趟扫完并按错误码分类"，仍走
//! `crate::cli::read_source` 的有损解码 + `(non-utf8)` 标记（design §3.5 末段的刻意保留）。

use crate::error::EngineError;
use std::path::Path;

/// 读入并解码源码文件；读不到即 `ENV-0010`（`std::io::Error` 的文本原样带出，与
/// `cli::read_source` 同形状——同一件事在四个消费面只能有一个码和一段文本）。
pub fn read_text(path: &Path) -> Result<String, EngineError> {
    let bytes = std::fs::read(path)
        .map_err(|e| EngineError::env(10, format!("read {}: {e}", path.display())))?;
    decode(&bytes, path)
}

/// 判定并解码（不碰磁盘，便于单测覆盖三种字节形态）。
///
/// UTF-8 合法一律原样返回——即使那串字节同时是合法 GBK，也不做第二种解释：交付件实测
/// UTF-8 与 GBK 二分（design §4），而一份真 UTF-8 中文文件被按 GBK 读会稳定产出乱码，
/// 所以"谁优先"不是风格问题。
pub fn decode(bytes: &[u8], path: &Path) -> Result<String, EngineError> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_owned());
    }
    let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if had_errors {
        return Err(refused(path));
    }
    Ok(text.into_owned())
}

/// 两条判定都不成立时的诊断（`ENV-0030` 形状与 D9① 逐字兼容：`读取 {path}: …` 前缀不变，
/// 尾巴诚实说明是两种编码都不合法，不再只报 UTF-8 那一条）。
fn refused(path: &Path) -> EngineError {
    EngineError::env(
        30,
        format!(
            "读取 {}: stream is neither valid UTF-8 nor valid GBK",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// "中文"的 GBK 字节（`0xD6 0xD0 0xCE 0xC4`），与 `tests/cli.rs` 夹具同一口径。
    const GBK_ZHONGWEN: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4];

    fn p() -> &'static Path {
        Path::new("x.sql")
    }

    #[test]
    fn should_pass_valid_utf8_bytes_through_byte_for_byte() {
        let bytes = "p_o := '中文';".as_bytes();
        assert_eq!(decode(bytes, p()).unwrap(), "p_o := '中文';");
    }

    #[test]
    fn should_decode_valid_gbk_bytes_to_their_unicode_text() {
        // D9② 的行为断言：GBK 中文字面量必须还原成对应汉字，而不是 U+FFFD 或报错。
        assert_eq!(decode(GBK_ZHONGWEN, p()).unwrap(), "中文");
    }

    #[test]
    fn should_refuse_bytes_that_are_valid_in_neither_encoding() {
        // 两种判定都不成立才是红：`0xFF`/`0xFE` 既不是合法 UTF-8 序列，也不是 GBK 的合法
        // 双字节（GBK 首字节 0x81–0xFE，且 0xFF 未定义）。绝不退回有损解码（D9① 消灭的形态）。
        let e = decode(b"p_o := '\xff\xfe';", p()).unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0030");
        assert!(e.message.contains("x.sql"), "{e}");
    }
}
