//! `compile --admit` 的判定块渲染与工件落盘（设计 v3.1 §3.2/§6）。
//!
//! 判定块插在 `compile:` 摘要行之后；`--admit=error` 的 firm skipped
//! 计数由调用方用于退出码（conservative/conditional 不判红，F2）。
//! admit 不发 TraceSink 事件、不物化 preludes、不执行例程体。

use super::compile_project::{AdmitRequest, AdmitRun};
use super::{io_err, skipped_reason_body};
use crate::engine::Engine;
use crate::error::EngineError;
use crate::ir::set_oriented::admit::{
    admit_verify, AdmissionVerdict, AdmitInput, Confidence, GuardOutcome, GuardState, Verdict,
};
use crate::ir::set_oriented::{GuardKind, GuardScope, SetOrientedMode};
use crate::project::ProjectConfig;
use crate::rules::RuleRegistry;
use std::io::Write;
use std::path::Path;

/// 跑判定并渲染 ADMIT 块；返回 firm `skipped` 数（`--admit=error` 判红面）。
/// EngineError（内核/环境失败）原样上抛——绝不产出半份判定。
pub(super) fn render_admit(
    project: &ProjectConfig,
    entries: &[String],
    run: &AdmitRun,
    engine: Option<&Engine>,
    out: &mut dyn Write,
) -> Result<usize, EngineError> {
    // mode=off 时集合化未启用，无判定对象——用法错而非产出误导性 verdict。
    if run.mode == SetOrientedMode::Off {
        return Err(EngineError::env(
            30,
            "compile --admit 需要非 off 的集合化模式（--set-oriented=strict|batch-atomic 或项目 [engine].set_oriented）",
        ));
    }
    let needs_kernel = match &run.request {
        AdmitRequest::Open { .. } => true,
        AdmitRequest::Input(input) => input.executor.is_some(),
    };
    // §3.1 约束 2：native 门显式降级标记，不假装门禁通过。
    if needs_kernel && !cfg!(feature = "native") {
        writeln!(out, "admit: skipped(no-native)").map_err(io_err)?;
        return Ok(0);
    }
    let Some(eng) = engine else {
        // 闭包无例程：无 catalog 可判，渲染空判定块。
        write_summary(&[], out)?;
        return Ok(0);
    };
    let opened;
    let input: &AdmitInput = match &run.request {
        AdmitRequest::Input(input) => input,
        AdmitRequest::Open { db } => {
            let db_str = db.as_ref().map(|p| p.to_string_lossy().into_owned());
            opened = AdmitInput::open(
                project,
                eng.catalog.sql_rules(),
                db_str.as_deref(),
                eng.macro_names().clone(),
            )?;
            &opened
        }
    };
    let verdicts = admit_verify(&eng.catalog, input, run.mode, entries)?;
    write_summary(&verdicts, out)?;
    for v in &verdicts {
        render_verdict(v, out)?;
        for g in &v.guards {
            render_guard(g, out)?;
        }
    }
    render_conclusion(&verdicts, out)?;
    if let Some(path) = &run.record {
        write_record(path, run, &verdicts, eng.catalog.sql_rules(), input)?;
    }
    Ok(firm_skipped(&verdicts))
}

fn firm_skipped(verdicts: &[AdmissionVerdict]) -> usize {
    verdicts
        .iter()
        .filter(|v| matches!(v.verdict, Verdict::Skipped(_)) && v.confidence == Confidence::Firm)
        .count()
}

/// 摘要行（§3.2 逐字规格 + static-only 计数：CLI 经 D9/注入面可达 StaticOnly）。
fn write_summary(verdicts: &[AdmissionVerdict], out: &mut dyn Write) -> Result<(), EngineError> {
    let converted = verdicts
        .iter()
        .filter(|v| v.verdict == Verdict::Converted)
        .count();
    let conditional = verdicts
        .iter()
        .filter(|v| v.verdict == Verdict::Conditional)
        .count();
    let static_only = verdicts
        .iter()
        .filter(|v| v.verdict == Verdict::StaticOnly)
        .count();
    let skipped = verdicts
        .iter()
        .filter(|v| matches!(v.verdict, Verdict::Skipped(_)) && v.confidence == Confidence::Firm)
        .count();
    let conservative = verdicts
        .iter()
        .filter(|v| {
            matches!(v.verdict, Verdict::Skipped(_)) && v.confidence == Confidence::Conservative
        })
        .count();
    let unchecked: usize = verdicts
        .iter()
        .map(|v| {
            v.guards
                .iter()
                .filter(|g| matches!(g.state, GuardState::Unchecked(_)))
                .count()
        })
        .sum();
    writeln!(
        out,
        "admit: targets={} converted={} conditional={} skipped={} conservative={} static-only={} unchecked={} native={}",
        verdicts.len(),
        converted,
        conditional,
        skipped,
        conservative,
        static_only,
        unchecked,
        if cfg!(feature = "native") { "on" } else { "off" },
    )
    .map_err(io_err)
}

/// 判定行：`skipped reason=X` 片段与 `call` 的 `SET_ORIENTED` 同源（§3.2 铁律）。
fn render_verdict(v: &AdmissionVerdict, out: &mut dyn Write) -> Result<(), EngineError> {
    match &v.verdict {
        Verdict::Converted => writeln!(out, "ADMIT {} converted", v.routine),
        Verdict::Conditional => writeln!(out, "ADMIT {} conditional", v.routine),
        Verdict::StaticOnly => writeln!(out, "ADMIT {} static-only", v.routine),
        Verdict::Skipped(reason) => match v.confidence {
            Confidence::Firm => {
                writeln!(out, "ADMIT {}", skipped_reason_body(&v.routine, *reason))
            }
            Confidence::Conservative => {
                writeln!(out, "ADMIT {} conservative reason={reason:?}", v.routine)
            }
        },
    }
    .map_err(io_err)
}

/// 守卫行：按 `entry_ok` 原始次序输出（不按 scope 分组，§3.2）。
fn render_guard(g: &GuardOutcome, out: &mut dyn Write) -> Result<(), EngineError> {
    let checked = match g.scope {
        GuardScope::Static => "compile",
        GuardScope::Frame => "frame",
    };
    let state = match g.state {
        GuardState::Passed => "passed",
        GuardState::Failed(_) => "failed",
        GuardState::Unchecked(_) => "unchecked",
    };
    let mut line = format!(
        "ADMIT   guard={} checked={} state={}",
        g.kind.cli_name(),
        checked,
        state
    );
    if let Some(object) = &g.object {
        line.push_str(&format!(" object={object}"));
    }
    if !g.binds.is_empty() {
        line.push_str(&format!(" binds={}", g.binds.join(",")));
    }
    match g.state {
        GuardState::Unchecked(reason) => {
            line.push_str(&format!(" reason={}", reason.as_str()));
        }
        _ => line.push_str(&format!(" expect=\"{}\"", guard_expect(g.kind))),
    }
    writeln!(out, "{line}").map_err(io_err)?;
    // 守卫 SQL 原文只在失败时随行展示（通过守卫的 SQL 属噪音）。
    if matches!(g.state, GuardState::Failed(_)) {
        if let Some(sql) = &g.sql {
            writeln!(out, "ADMIT     sql={sql}").map_err(io_err)?;
        }
    }
    Ok(())
}

/// 守卫期望话术（§3.2 示例口径；话术层不参与契约断言，D5）。
fn guard_expect(kind: GuardKind) -> &'static str {
    match kind {
        GuardKind::SysDummy => "恰 1 行",
        GuardKind::Drift => "列集/限定名与生成期证据一致",
        GuardKind::Snapshot => "与编译期快照一致",
        GuardKind::Top1Tie => "组内并列键值一致",
        GuardKind::Route => "THEN 分支不可达",
        GuardKind::ExprTotalness => "全定义（无 NULL/报错）",
    }
}

/// 末行「结论」话术（D5）：仅 skipped 且存在已求值失败的守卫时打。
fn render_conclusion(
    verdicts: &[AdmissionVerdict],
    out: &mut dyn Write,
) -> Result<(), EngineError> {
    let hit = verdicts.iter().any(|v| {
        matches!(v.verdict, Verdict::Skipped(_))
            && v.guards
                .iter()
                .any(|g| matches!(g.state, GuardState::Failed(_)))
    });
    if hit {
        writeln!(
            out,
            "admit: 结论 = 当前数据下不可集合化；调整数据或例程后重跑 compile --admit 复核"
        )
        .map_err(io_err)?;
    }
    Ok(())
}

/// `--admit-record` 工件（§6 `spinduck-admit/1`）：显式给路径才写，不隐式写项目树。
fn write_record(
    path: &Path,
    run: &AdmitRun,
    verdicts: &[AdmissionVerdict],
    rules: &RuleRegistry,
    input: &AdmitInput,
) -> Result<(), EngineError> {
    let mut s = String::from("# spinduck-admit/1\n[provenance]\n");
    s.push_str(&format!("binary = \"spinduck {}\"\n", crate::version()));
    s.push_str(&format!("kernel = \"{}\"\n", rules.target_duckdb));
    s.push_str(&format!("rules_version = \"{}\"\n", rules.target_duckdb));
    s.push_str(&format!("mode = \"{}\"\n", run.mode.as_str()));
    let db_label = match &run.request {
        AdmitRequest::Open { db: Some(db) } => db.to_string_lossy().into_owned(),
        AdmitRequest::Open { db: None } => "none".to_string(),
        AdmitRequest::Input(_) if input.executor.is_some() => "injected".to_string(),
        AdmitRequest::Input(_) => "none".to_string(),
    };
    s.push_str(&format!("db = \"{}\"\n", toml_str(&db_label)));
    for v in verdicts {
        s.push_str("\n[[routine]]\n");
        s.push_str(&format!("name = \"{}\"\n", toml_str(&v.routine)));
        s.push_str(&format!("verdict = \"{}\"\n", v.verdict.as_str()));
        s.push_str(&format!("confidence = \"{}\"\n", v.confidence.as_str()));
        if let Verdict::Skipped(reason) = v.verdict {
            s.push_str(&format!("reason = \"{}\"\n", reason.as_str()));
        }
        if let Some(note) = &v.note {
            s.push_str(&format!("note = \"{}\"\n", toml_str(note)));
        }
        for g in &v.guards {
            s.push_str("\n[[routine.guard]]\n");
            s.push_str(&format!("kind = \"{}\"\n", g.kind.as_str()));
            s.push_str(&format!(
                "scope = \"{}\"\n",
                match g.scope {
                    GuardScope::Static => "static",
                    GuardScope::Frame => "frame",
                }
            ));
            let state = match g.state {
                GuardState::Passed => "passed",
                GuardState::Failed(_) => "failed",
                GuardState::Unchecked(_) => "unchecked",
            };
            s.push_str(&format!("state = \"{state}\"\n"));
            if let GuardState::Unchecked(r) = g.state {
                s.push_str(&format!("reason = \"{}\"\n", r.as_str()));
            }
            if let Some(object) = &g.object {
                s.push_str(&format!("object = \"{}\"\n", toml_str(object)));
            }
            if !g.binds.is_empty() {
                let binds: Vec<String> = g
                    .binds
                    .iter()
                    .map(|b| format!("\"{}\"", toml_str(b)))
                    .collect();
                s.push_str(&format!("binds = [{}]\n", binds.join(", ")));
            }
            if let Some(sql) = &g.sql {
                if !matches!(g.state, GuardState::Passed) {
                    s.push_str(&format!("sql = \"{}\"\n", toml_str(sql)));
                }
            }
            if !matches!(g.state, GuardState::Unchecked(_)) {
                s.push_str(&format!("expect = \"{}\"\n", guard_expect(g.kind)));
            }
        }
    }
    std::fs::write(path, s)
        .map_err(|e| EngineError::env(12, format!("admit-record {}: {e}", path.display())))
}

/// TOML 基本字符串转义（`\` `"` 与控制字符）。
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}
