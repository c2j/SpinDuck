//! `call --trace` 的落盘装配：本模块只管生命周期，渲染与级别过滤全在 `src/trace/`。
//!
//! 三条纪律在此落地：日志文件在**缺件门之后**才创建（拒绝执行不该留下"跑过什么"的
//! 假象）；时钟与 sid/cid 由运行环境注入（AGENTS §2.6 接缝 3，`src/trace/` 自己不信时钟）；
//! 日志通道的写失败必须返回错误而不是静默少一份审计（§11.1 禁止静默丢弃）。

use crate::error::EngineError;
use crate::exec::TraceSink;
use crate::trace::{call_id, session_id, utc_millis, AuditSink, SinkCfg, TraceFormat, TraceLevel};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 一次 `--trace` 的运行：构造即打开日志文件，`finish` 收尾写 sidecar 并上报写失败。
pub struct TraceRun {
    sidecar: PathBuf,
    sink: AuditSink<std::fs::File>,
}

impl TraceRun {
    /// 打开 `<path>` 承接审计日志。`proc` 写进行头的 `proc` 字段，即本次调用的目标例程。
    pub fn open(
        path: &Path,
        fmt: TraceFormat,
        level: TraceLevel,
        proc: &str,
    ) -> Result<Self, EngineError> {
        let file = std::fs::File::create(path)
            .map_err(|e| EngineError::env(12, format!("trace log {}: {e}", path.display())))?;
        let sid = session_id(std::process::id(), epoch_millis());
        let sink = AuditSink::new(
            SinkCfg {
                fmt,
                level,
                sid,
                // 一次进程一条 CALL：cid 恒为第 1 次，与 sid 拼起来即全局唯一
                cid: call_id(1),
                proc: proc.to_string(),
            },
            || utc_millis(epoch_millis()),
            file,
        );
        Ok(Self {
            sidecar: sidecar_of(path),
            sink,
        })
    }

    /// 交给引擎的事件汇。
    pub fn sink(&mut self) -> &mut dyn TraceSink {
        &mut self.sink
    }

    /// 收尾：sidecar（sqlid → 全文）落盘，再把日志通道的首因写失败上报。
    pub fn finish(self) -> Result<(), EngineError> {
        let Self { sidecar, sink } = self;
        let entries: Vec<(String, String)> = sink
            .sql_index()
            .iter()
            .map(|(id, sql)| (id.clone(), sql.clone()))
            .collect();
        write_sidecar(&sidecar, &entries)?;
        match sink.io_error() {
            Some(e) => Err(EngineError::env(
                13,
                format!("审计日志通道失效（{sidecar:?} 同前缀）: {e}"),
            )),
            None => Ok(()),
        }
    }
}

/// sidecar 与主日志同前缀：`run.log` → `run.log.sql`（扩展名不参与，避免改名错位）。
fn sidecar_of(path: &Path) -> PathBuf {
    let mut side = path.as_os_str().to_os_string();
    side.push(".sql");
    PathBuf::from(side)
}

/// `sqlid<TAB>归一全文` 一行一条；全文已折叠空白，故 grep 结果单行可读。
fn write_sidecar(path: &Path, entries: &[(String, String)]) -> Result<(), EngineError> {
    let mut file = std::fs::File::create(path)
        .map_err(|e| EngineError::env(12, format!("trace sidecar {}: {e}", path.display())))?;
    for (id, sql) in entries {
        writeln!(file, "{id}\t{sql}")
            .map_err(|e| EngineError::env(13, format!("sidecar {path:?}: {e}")))?;
    }
    file.flush()
        .map_err(|e| EngineError::env(13, format!("sidecar {path:?}: {e}")))
}

/// 系统时钟的 epoch 毫秒；时钟早于 1970 时退化为 0（不因此中断审计）。
fn epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}
