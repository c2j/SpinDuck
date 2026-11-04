//! 例程目录（§3.6）：已编译过程/函数的注册表与 CALL 结果契约。
//! 持久化（DuckDB 内部表）在 M7 接入；本模块提供内存目录与编译入口。

use crate::error::EngineError;
use crate::ir::{compile_routine_with, IrProg};
use crate::rules::RuleRegistry;
use crate::value::SpinValue;
use ogsql_parser::ast::Statement;
use std::collections::BTreeMap;

/// 参数模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamMode {
    In,
    Out,
    InOut,
}

/// 例程参数。
#[derive(Debug, Clone)]
pub struct RoutineParam {
    pub name: String,
    pub mode: ParamMode,
}

/// 已编译例程。
#[derive(Debug, Clone)]
pub struct CompiledRoutine {
    pub name: String,
    pub kind: RoutineKind,
    pub params: Vec<RoutineParam>,
    pub prog: IrProg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineKind {
    Procedure,
    Function,
}

/// CALL 结果（§12 P5 错误契约：OUT 参数按名取回，返回标量直接取）。
#[derive(Debug, Clone, Default)]
pub struct CallResult {
    pub outs: BTreeMap<String, SpinValue>,
    pub return_value: Option<SpinValue>,
}

/// 内存例程目录。
#[derive(Debug, Default, Clone)]
pub struct RoutineCatalog {
    routines: BTreeMap<String, CompiledRoutine>,
}

impl RoutineCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, r: CompiledRoutine) {
        self.routines.insert(r.name.to_ascii_lowercase(), r);
    }

    pub fn lookup(&self, name: &str) -> Option<&CompiledRoutine> {
        self.routines.get(&name.to_ascii_lowercase())
    }

    pub fn len(&self) -> usize {
        self.routines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routines.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.routines.keys().cloned().collect()
    }
}

/// ogsql `RoutineParam.mode: Option<String>` → ParamMode。
pub fn map_mode_pub(mode: &Option<String>) -> ParamMode {
    map_mode(mode)
}

fn map_mode(mode: &Option<String>) -> ParamMode {
    match mode
        .as_deref()
        .unwrap_or("IN")
        .to_ascii_uppercase()
        .as_str()
    {
        s if s.contains("OUT") => ParamMode::Out,
        s if s.contains("IN") => ParamMode::In,
        _ => ParamMode::In,
    }
}

/// 从 ogsql AST 语句编译例程并登记（CREATE PROCEDURE / CREATE FUNCTION）。
/// PACKAGE spec/body 的展开在 engine 层完成。
pub fn compile_statement_routine(
    st: &Statement,
    rules: &RuleRegistry,
) -> Result<Vec<CompiledRoutine>, EngineError> {
    let mut out = Vec::new();
    match st {
        Statement::CreateProcedure(cs) => {
            let extra: Vec<String> = cs
                .parameters
                .iter()
                .map(|p| p.name.to_ascii_lowercase())
                .collect();
            let prog = match &cs.block {
                Some(b) => compile_routine_with(b, &extra, rules)?,
                None => IrProg::default(),
            };
            let params = cs
                .parameters
                .iter()
                .map(|p| RoutineParam {
                    name: p.name.clone(),
                    mode: map_mode(&p.mode),
                })
                .collect();
            out.push(CompiledRoutine {
                name: cs
                    .name
                    .iter()
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join("."),
                kind: RoutineKind::Procedure,
                params,
                prog,
            });
        }
        Statement::CreateFunction(cf) => {
            let extra: Vec<String> = cf
                .parameters
                .iter()
                .map(|p| p.name.to_ascii_lowercase())
                .collect();
            let prog = match &cf.block {
                Some(b) => compile_routine_with(b, &extra, rules)?,
                None => IrProg::default(),
            };
            let params = cf
                .parameters
                .iter()
                .map(|p| RoutineParam {
                    name: p.name.clone(),
                    mode: map_mode(&p.mode),
                })
                .collect();
            out.push(CompiledRoutine {
                name: cf
                    .name
                    .iter()
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join("."),
                kind: RoutineKind::Function,
                params,
                prog,
            });
        }
        other => {
            return Err(EngineError::invariant(
                60,
                format!("compile_statement_routine: unsupported statement {other:?}"),
            ));
        }
    }
    Ok(out)
}
