//! `verify` 子命令壳：定位项目 → 运行态快照 → 项目态 compile（§2.1：CLI 只做壳）。
//!
//! 不再调用 `verify::verify_project`。场景 PASS/SKIP 不在这条输出里。

use crate::error::EngineError;
use crate::plan::codeweb::CodewebExport;
use crate::project::ProjectConfig;
use std::io::Write;
use std::path::Path;

/// `spinduck verify [项目目录]`：目录缺省为当前工作目录。
///
/// 保留快照同步与项目定位。链路与场景渲染改走项目态 compile（全部 `[[entry]]`）。
pub fn verify_cmd(
    path: Option<&Path>,
    export: &impl CodewebExport,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let start = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| EngineError::env(12, format!("cwd: {e}")))?,
    };
    let project = locate(&start)?;
    // 没有入口就还不写快照：运行态不能声称一个没被点名的可调用面
    let entries = crate::cli::run::project_entries(&project)?;
    sync_manifest(&project, out)?;
    crate::cli::run::compile_with_export(&project, &entries, None, export, out)
}

/// 运行态快照同步（§11.1 `conf/`）。
///
/// 只在研发态目录（含手写 `spinduck.toml`）写入：从运行态目录执行时不回写，
/// 快照的来源标注因此始终指向真源，"漂移由下一次 verify 判读"的口径才不会断。
fn sync_manifest(project: &ProjectConfig, out: &mut dyn Write) -> Result<(), EngineError> {
    if project.loaded_as_runtime {
        let path = project.home_dir.join("conf").join("spinduck.toml");
        writeln!(
            out,
            "manifest: {} (未改写：本目录即运行态快照)",
            path.display()
        )
        .map_err(io_err)?;
        return Ok(());
    }
    let line = if project.root.join("spinduck.toml").is_file() {
        let s = crate::verify::sync_runtime_manifest(project)?;
        format!(
            "manifest: {} ({})",
            s.path.display(),
            if s.changed { "updated" } else { "unchanged" }
        )
    } else {
        format!(
            "manifest: {} (未改写：本目录即运行态快照)",
            project.root.join("conf/spinduck.toml").display()
        )
    };
    writeln!(out, "{line}").map_err(io_err)
}

/// 项目定位：目录本身有声明文件（研发态或运行态快照）直接用，否则向上查找。
///
/// `plan` 复用同一口径（`src/cli/plan.rs`）：两个入口都从项目声明出发，不接受散落参数。
pub(crate) fn locate(start: &Path) -> Result<ProjectConfig, EngineError> {
    if start.join("spinduck.toml").is_file() || start.join("conf/spinduck.toml").is_file() {
        return ProjectConfig::load(start);
    }
    ProjectConfig::discover(start).ok_or_else(|| {
        // F7：verify/plan 共用本入口，消息不绑定子命令名（各自 usage 已说明起点语义）
        EngineError::env(
            30,
            format!(
                "未在 {} 及其上级目录找到项目声明（spinduck.toml 或 conf/spinduck.toml）",
                start.display()
            ),
        )
    })
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}

#[cfg(test)]
mod tests {
    use super::locate;
    use crate::cli::run::run;

    #[test]
    fn should_locate_error_mention_only_project_declaration() {
        // F7：locate 为 verify/plan 共用，错误消息不得绑定具体子命令名
        let e = locate(std::path::Path::new("/nonexistent/spinduck-f7")).unwrap_err();
        let msg = e.message;
        assert!(msg.contains("项目声明"), "{msg}");
        assert!(!msg.contains("verify"), "消息不得带 verify 字样: {msg}");
    }

    #[test]
    fn should_plan_error_reuse_neutral_locate_message() {
        // F7：plan 无项目声明时走同一消息（全链路经 run()，退出码 1）
        let mut buf: Vec<u8> = Vec::new();
        let argv = vec!["plan".to_string(), "fnc_x".to_string()];
        let code = run(&argv, &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("项目声明"), "{out}");
        assert!(!out.contains("verify 以项目声明为起点"), "{out}");
    }
}
