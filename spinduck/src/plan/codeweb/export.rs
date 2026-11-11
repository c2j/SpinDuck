//! 进程外 codeweb 0.9.7：先核对版本和 diff，再把 JSON 导出接到管道上。
//!
//! 参数是 `Command` 的 argv，不经 shell。读完 stdout 之前不丢 `Child`，否则管道会被提前关掉。

use crate::error::EngineError;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const VERSION_WORD: &str = "0.9.7";
const DIFF_SENTINEL: &str = "Up to date. No changes detected.";
/// 失败消息里附带的 stderr 上限。成功路径的 stderr 直接丢掉。
const STDERR_LIMIT: usize = 2048;

/// 进程外导出。测试用夹具实现，生产实现才 `Command::new`。
///
/// 返回值在读完之前必须持有子进程：把 `Child` 和 stdout 包在同一个 `Read` 里，
/// 读完或丢弃时才 `wait`。
pub trait CodewebExport {
    fn export_json(&self, project: &Path) -> Result<Box<dyn std::io::Read + Send>, EngineError>;
}

/// `--codeweb-bin`，然后 `SPINDUCK_CODEWEB`，然后 `PATH` 上的 `codeweb`。命中即停。
pub(crate) fn resolve_codeweb_bin(
    flag: Option<&Path>,
    from_env: Option<&str>,
    path_var: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, EngineError> {
    if let Some(path) = flag {
        return existing_bin(path);
    }
    if let Some(raw) = from_env.map(str::trim).filter(|s| !s.is_empty()) {
        return existing_bin(Path::new(raw));
    }
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join("codeweb");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(missing_bin(
        "未找到 codeweb（--codeweb-bin、SPINDUCK_CODEWEB、PATH）",
    ))
}

/// 生产导出：一次 `export_json` 里顺序跑 `--version`、`diff`、`export`。
pub(crate) struct CommandExport {
    bin: PathBuf,
}

impl CommandExport {
    pub(crate) fn new(bin: PathBuf) -> Self {
        Self { bin }
    }
}

impl CodewebExport for CommandExport {
    fn export_json(&self, project: &Path) -> Result<Box<dyn std::io::Read + Send>, EngineError> {
        check_version(&self.bin)?;
        check_diff(&self.bin, project)?;
        spawn_export(&self.bin, project)
    }
}

fn existing_bin(path: &Path) -> Result<PathBuf, EngineError> {
    if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(missing_bin(path.display()))
    }
}

fn missing_bin(detail: impl std::fmt::Display) -> EngineError {
    EngineError::env(30, format!("codeweb 二进制不可用: {detail}"))
}

fn check_version(bin: &Path) -> Result<(), EngineError> {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(missing_bin)?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(version_err(format!(
            "exit {code}: {}",
            tail_text(&out.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !has_version_word(&stdout) {
        return Err(version_err(tail_text(&out.stdout)));
    }
    Ok(())
}

fn check_diff(bin: &Path, project: &Path) -> Result<(), EngineError> {
    let out = Command::new(bin)
        .arg("diff")
        .arg("--project")
        .arg(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| EngineError::env(30, format!("codeweb diff 失败: {e}")))?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(EngineError::env(
            30,
            format!("codeweb diff 失败: exit {code}: {}", tail_text(&out.stderr)),
        ));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if strip_one_trailing_newline(&stderr) != DIFF_SENTINEL {
        return Err(EngineError::env(
            30,
            format!("codeweb 图与源码不一致: {}", tail_text(&out.stderr)),
        ));
    }
    Ok(())
}

fn spawn_export(bin: &Path, project: &Path) -> Result<Box<dyn Read + Send>, EngineError> {
    let mut child = Command::new(bin)
        .arg("export")
        .arg("--format")
        .arg("json")
        .arg("--project")
        .arg(project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| EngineError::env(30, format!("codeweb export 失败: {e}")))?;
    let stdout = take_pipe(child.stdout.take(), "stdout")?;
    let stderr = take_pipe(child.stderr.take(), "stderr")?;
    let stderr_task = std::thread::spawn(move || read_stderr_tail(stderr));
    Ok(Box::new(OwnedExport {
        child: Some(child),
        stdout: Some(stdout),
        stderr: Some(stderr_task),
        reported: false,
    }))
}

fn take_pipe<T>(pipe: Option<T>, name: &str) -> Result<T, EngineError> {
    pipe.ok_or_else(|| EngineError::env(30, format!("codeweb export 失败: 没有 {name} 管道")))
}

fn version_err(detail: impl std::fmt::Display) -> EngineError {
    EngineError::env(30, format!("codeweb 版本不是 {VERSION_WORD}: {detail}"))
}

fn has_version_word(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .any(|word| word == VERSION_WORD)
}

/// 只去掉一个末尾换行（`\n` 或 `\r\n`），不吞掉正文里的空行。
fn strip_one_trailing_newline(text: &str) -> &str {
    match text.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => text.strip_suffix('\r').unwrap_or(text),
    }
}

fn tail_text(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_LIMIT);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn read_stderr_tail(mut input: impl Read) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => push_tail(&mut tail, &buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    tail
}

fn push_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    if tail.len() > STDERR_LIMIT {
        let drop_n = tail.len() - STDERR_LIMIT;
        tail.drain(..drop_n);
    }
}

/// stdout 读完（或这个值被丢弃）之后才 `wait`。中途丢掉 `Child` 既收不到退出码，也会拆掉管道。
struct OwnedExport {
    child: Option<Child>,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
    reported: bool,
}

impl Read for OwnedExport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = read_stdout(&mut self.stdout, buf)?;
        if n == 0 {
            self.finish()?;
        }
        Ok(n)
    }
}

impl Drop for OwnedExport {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn read_stdout(
    stdout: &mut Option<std::process::ChildStdout>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    let Some(pipe) = stdout.as_mut() else {
        return Ok(0);
    };
    loop {
        match pipe.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

impl OwnedExport {
    fn finish(&mut self) -> std::io::Result<()> {
        if self.reported {
            return Ok(());
        }
        self.reported = true;
        self.stdout.take();
        let stderr = match self.stderr.take() {
            Some(handle) => handle.join().unwrap_or_default(),
            None => Vec::new(),
        };
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let status = child
            .wait()
            .map_err(|e| std::io::Error::other(format!("codeweb export 失败: {e}")))?;
        if status.success() {
            return Ok(());
        }
        let code = status.code().unwrap_or(-1);
        Err(std::io::Error::other(format!(
            "codeweb export 失败: exit {code}: {}",
            tail_text(&stderr)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_treat_only_a_standalone_0_9_7_token_as_the_version() {
        assert!(has_version_word("codeweb 0.9.7\n"));
        assert!(has_version_word("codeweb 0.9.7 (rev)"));
        assert!(!has_version_word("codeweb 0.9.70\n"));
        assert!(!has_version_word("codeweb 10.9.7\n"));
        assert!(!has_version_word("v0.9.7\n"));
    }

    #[test]
    fn should_keep_only_the_last_two_kib_of_captured_stderr() {
        let bytes = vec![b'a'; STDERR_LIMIT + 100];
        let text = tail_text(&bytes);
        assert_eq!(text.len(), STDERR_LIMIT);
        assert!(text.chars().all(|c| c == 'a'));
    }

    #[test]
    fn should_resolve_codeweb_bin_flag_then_env_then_path() {
        let tmp = tempfile::tempdir().unwrap();
        let flagged = tmp.path().join("flagged");
        let from_env = tmp.path().join("from-env");
        let on_path = tmp.path().join("bin");
        std::fs::create_dir_all(&on_path).unwrap();
        std::fs::write(&flagged, b"").unwrap();
        std::fs::write(&from_env, b"").unwrap();
        std::fs::write(on_path.join("codeweb"), b"").unwrap();
        let path = std::env::join_paths([&on_path]).unwrap();

        let hit = resolve_codeweb_bin(
            Some(&flagged),
            Some(from_env.to_str().unwrap()),
            Some(&path),
        )
        .unwrap();
        assert_eq!(hit, flagged);
        let hit = resolve_codeweb_bin(None, Some(from_env.to_str().unwrap()), Some(&path)).unwrap();
        assert_eq!(hit, from_env);
        let hit = resolve_codeweb_bin(None, Some("  "), Some(&path)).unwrap();
        assert_eq!(hit, on_path.join("codeweb"));
        let missing = tmp.path().join("absent");
        let err = resolve_codeweb_bin(
            Some(&missing),
            Some(from_env.to_str().unwrap()),
            Some(&path),
        )
        .unwrap_err();
        assert!(
            err.message.starts_with("codeweb 二进制不可用:"),
            "{}",
            err.message
        );
        let err = resolve_codeweb_bin(None, None, Some(std::ffi::OsStr::new(""))).unwrap_err();
        assert!(
            err.message.starts_with("codeweb 二进制不可用:"),
            "{}",
            err.message
        );
    }
}
